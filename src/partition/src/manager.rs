// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use api::v1::RowInsertRequest;
use common_meta::peer::Peer;
use common_meta::rpc::router::TableRoute;
use common_query::prelude::Expr;
use datafusion_expr::{BinaryExpr, Expr as DfExpr, Operator};
use datatypes::prelude::Value;
use datatypes::schema::Schema;
use snafu::{ensure, OptionExt, ResultExt};
use store_api::storage::{RegionId, RegionNumber};
use table::metadata::TableId;
use table::requests::{DeleteRequest, InsertRequest};

use crate::columns::RangeColumnsPartitionRule;
use crate::error::{FindLeaderSnafu, Result};
use crate::partition::{PartitionBound, PartitionDef, PartitionExpr};
use crate::range::RangePartitionRule;
use crate::route::TableRoutes;
use crate::row_splitter::{RowInsertRequestSplits, RowSplitter};
use crate::splitter::{DeleteRequestSplit, InsertRequestSplit, WriteSplitter};
use crate::{error, PartitionRuleRef};

#[async_trait::async_trait]
pub trait TableRouteCacheInvalidator: Send + Sync {
    async fn invalidate_table_route(&self, table: TableId);
}

pub type TableRouteCacheInvalidatorRef = Arc<dyn TableRouteCacheInvalidator>;

pub type PartitionRuleManagerRef = Arc<PartitionRuleManager>;

/// PartitionRuleManager manages the table routes and partition rules.
/// It provides methods to find regions by:
/// - values (in case of insertion)
/// - filters (in case of select, deletion and update)
pub struct PartitionRuleManager {
    table_routes: Arc<TableRoutes>,
}

#[derive(Debug)]
pub struct PartitionInfo {
    pub id: RegionId,
    pub partition: PartitionDef,
}

#[async_trait::async_trait]
impl TableRouteCacheInvalidator for PartitionRuleManager {
    async fn invalidate_table_route(&self, table: TableId) {
        self.table_routes.invalidate_table_route(table).await
    }
}

impl PartitionRuleManager {
    pub fn new(table_routes: Arc<TableRoutes>) -> Self {
        Self { table_routes }
    }

    pub fn table_routes(&self) -> &TableRoutes {
        self.table_routes.as_ref()
    }

    /// Find table route of given table name.
    pub async fn find_table_route(&self, table: TableId) -> Result<Arc<TableRoute>> {
        self.table_routes.get_route(table).await
    }

    /// Find datanodes of corresponding regions of given table.
    pub async fn find_region_datanodes(
        &self,
        table_id: TableId,
        regions: Vec<RegionNumber>,
    ) -> Result<HashMap<Peer, Vec<RegionNumber>>> {
        let route = self.table_routes.get_route(table_id).await?;
        let mut datanodes = HashMap::with_capacity(regions.len());
        for region in regions.iter() {
            let datanode = route
                .find_region_leader(*region)
                .context(error::FindDatanodeSnafu {
                    table_id,
                    region: *region,
                })?;
            datanodes
                .entry(datanode.clone())
                .or_insert_with(Vec::new)
                .push(*region);
        }
        Ok(datanodes)
    }

    /// Find all leader peers of given table.
    pub async fn find_table_region_leaders(&self, table_id: TableId) -> Result<Vec<Peer>> {
        let route = self.table_routes.get_route(table_id).await?;
        let mut peers = Vec::with_capacity(route.region_routes.len());
        for peer in &route.region_routes {
            peers.push(peer.leader_peer.clone().with_context(|| FindLeaderSnafu {
                region_id: peer.region.id,
                table_id,
            })?);
        }

        Ok(peers)
    }

    pub async fn find_table_partitions(&self, table_id: TableId) -> Result<Vec<PartitionInfo>> {
        let route = self.table_routes.get_route(table_id).await?;
        ensure!(
            !route.region_routes.is_empty(),
            error::FindTableRoutesSnafu { table_id }
        );

        let mut partitions = Vec::with_capacity(route.region_routes.len());
        for r in route.region_routes.iter() {
            let partition = r
                .region
                .partition
                .clone()
                .context(error::FindRegionRoutesSnafu {
                    region_id: r.region.id,
                    table_id,
                })?;
            let partition_def = PartitionDef::try_from(partition)?;

            partitions.push(PartitionInfo {
                id: r.region.id,
                partition: partition_def,
            });
        }
        partitions.sort_by(|a, b| {
            a.partition
                .partition_bounds()
                .cmp(b.partition.partition_bounds())
        });

        ensure!(
            partitions
                .windows(2)
                .all(|w| w[0].partition.partition_columns() == w[1].partition.partition_columns()),
            error::InvalidTableRouteDataSnafu {
                table_id,
                err_msg: "partition columns of all regions are not the same"
            }
        );

        Ok(partitions)
    }

    /// Get partition rule of given table.
    pub async fn find_table_partition_rule(&self, table_id: TableId) -> Result<PartitionRuleRef> {
        let partitions = self.find_table_partitions(table_id).await?;

        let partition_columns = partitions[0].partition.partition_columns();

        let regions = partitions
            .iter()
            .map(|x| x.id.region_number())
            .collect::<Vec<RegionNumber>>();

        // TODO(LFC): Serializing and deserializing partition rule is ugly, must find a much more elegant way.
        let partition_rule: PartitionRuleRef = match partition_columns.len() {
            1 => {
                // Omit the last "MAXVALUE".
                let bounds = partitions
                    .iter()
                    .filter_map(|info| match &info.partition.partition_bounds()[0] {
                        PartitionBound::Value(v) => Some(v.clone()),
                        PartitionBound::MaxValue => None,
                    })
                    .collect::<Vec<Value>>();
                Arc::new(RangePartitionRule::new(
                    partition_columns[0].clone(),
                    bounds,
                    regions,
                )) as _
            }
            _ => {
                let bounds = partitions
                    .iter()
                    .map(|x| x.partition.partition_bounds().clone())
                    .collect::<Vec<Vec<PartitionBound>>>();
                Arc::new(RangeColumnsPartitionRule::new(
                    partition_columns.clone(),
                    bounds,
                    regions,
                )) as _
            }
        };
        Ok(partition_rule)
    }

    /// Find regions in partition rule by filters.
    pub fn find_regions_by_filters(
        &self,
        partition_rule: PartitionRuleRef,
        filters: &[Expr],
    ) -> Result<Vec<RegionNumber>> {
        let regions = if let Some((first, rest)) = filters.split_first() {
            let mut target = find_regions0(partition_rule.clone(), first)?;
            for filter in rest {
                let regions = find_regions0(partition_rule.clone(), filter)?;

                // When all filters are provided as a collection, it often implicitly states that
                // "all filters must be satisfied". So we join all the results here.
                target.retain(|x| regions.contains(x));

                // Failed fast, empty collection join any is empty.
                if target.is_empty() {
                    break;
                }
            }
            target.into_iter().collect::<Vec<_>>()
        } else {
            partition_rule.find_regions_by_exprs(&[])?
        };
        ensure!(
            !regions.is_empty(),
            error::FindRegionsSnafu {
                filters: filters.to_vec()
            }
        );
        Ok(regions)
    }

    /// Split [InsertRequest] into [InsertRequestSplit] according to the partition rule
    /// of given table.
    pub async fn split_insert_request(
        &self,
        table: TableId,
        req: InsertRequest,
        schema: &Schema,
    ) -> Result<InsertRequestSplit> {
        let partition_rule = self.find_table_partition_rule(table).await?;
        let splitter = WriteSplitter::with_partition_rule(partition_rule);
        splitter.split_insert(req, schema)
    }

    /// Split [RowInsertRequest] into [RowInsertRequestSplits] according to the partition rule
    /// of given table.
    pub async fn split_row_insert_request(
        &self,
        table: TableId,
        req: RowInsertRequest,
    ) -> Result<RowInsertRequestSplits> {
        let partition_rule = self.find_table_partition_rule(table).await?;
        RowSplitter::new(partition_rule).split(req)
    }

    pub async fn split_delete_request(
        &self,
        table: TableId,
        req: DeleteRequest,
        primary_key_column_names: Vec<&String>,
    ) -> Result<DeleteRequestSplit> {
        let partition_rule = self.find_table_partition_rule(table).await?;
        let splitter = WriteSplitter::with_partition_rule(partition_rule);
        splitter.split_delete(req, primary_key_column_names)
    }
}

fn find_regions0(partition_rule: PartitionRuleRef, filter: &Expr) -> Result<HashSet<RegionNumber>> {
    let expr = filter.df_expr();
    match expr {
        DfExpr::BinaryExpr(BinaryExpr { left, op, right }) if is_compare_op(op) => {
            let column_op_value = match (left.as_ref(), right.as_ref()) {
                (DfExpr::Column(c), DfExpr::Literal(v)) => Some((&c.name, *op, v)),
                (DfExpr::Literal(v), DfExpr::Column(c)) => Some((&c.name, reverse_operator(op), v)),
                _ => None,
            };
            if let Some((column, op, scalar)) = column_op_value {
                let value = Value::try_from(scalar.clone()).with_context(|_| {
                    error::ConvertScalarValueSnafu {
                        value: scalar.clone(),
                    }
                })?;
                return Ok(partition_rule
                    .find_regions_by_exprs(&[PartitionExpr::new(column, op, value)])?
                    .into_iter()
                    .collect::<HashSet<RegionNumber>>());
            }
        }
        DfExpr::BinaryExpr(BinaryExpr { left, op, right })
            if matches!(op, Operator::And | Operator::Or) =>
        {
            let left_regions = find_regions0(partition_rule.clone(), &(*left.clone()).into())?;
            let right_regions = find_regions0(partition_rule.clone(), &(*right.clone()).into())?;
            let regions = match op {
                Operator::And => left_regions
                    .intersection(&right_regions)
                    .cloned()
                    .collect::<HashSet<RegionNumber>>(),
                Operator::Or => left_regions
                    .union(&right_regions)
                    .cloned()
                    .collect::<HashSet<RegionNumber>>(),
                _ => unreachable!(),
            };
            return Ok(regions);
        }
        _ => (),
    }

    // Returns all regions for not supported partition expr as a safety hatch.
    Ok(partition_rule
        .find_regions_by_exprs(&[])?
        .into_iter()
        .collect::<HashSet<RegionNumber>>())
}

#[inline]
fn is_compare_op(op: &Operator) -> bool {
    matches!(
        *op,
        Operator::Eq
            | Operator::NotEq
            | Operator::Lt
            | Operator::LtEq
            | Operator::Gt
            | Operator::GtEq
    )
}

#[inline]
fn reverse_operator(op: &Operator) -> Operator {
    match *op {
        Operator::Lt => Operator::Gt,
        Operator::Gt => Operator::Lt,
        Operator::LtEq => Operator::GtEq,
        Operator::GtEq => Operator::LtEq,
        _ => *op,
    }
}
