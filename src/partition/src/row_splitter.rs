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

use std::collections::HashMap;

use api::helper;
use api::v1::{ColumnSchema, Row, RowInsertRequest, Rows};
use datatypes::value::Value;
use store_api::storage::RegionNumber;

use crate::error::Result;
use crate::PartitionRuleRef;

pub type RowInsertRequestSplits = HashMap<RegionNumber, RowInsertRequest>;

pub struct RowSplitter {
    partition_rule: PartitionRuleRef,
}

impl RowSplitter {
    pub fn new(partition_rule: PartitionRuleRef) -> Self {
        Self { partition_rule }
    }

    pub fn split(&self, req: RowInsertRequest) -> Result<RowInsertRequestSplits> {
        // No partition
        let partition_columns = self.partition_rule.partition_columns();
        if partition_columns.is_empty() {
            return Ok(HashMap::from([(0, req)]));
        }

        // No data
        let Some(rows) = req.rows else {
            return Ok(HashMap::new());
        };

        SplitReadRowHelper::new(req.table_name, rows, &self.partition_rule).split_to_requests()
    }
}

struct SplitReadRowHelper<'a> {
    table_name: String,
    schema: Vec<ColumnSchema>,
    rows: Vec<Row>,
    partition_rule: &'a PartitionRuleRef,
    // Map from partition column name to index in the schema/row.
    partition_cols_indexes: Vec<Option<usize>>,
}

impl<'a> SplitReadRowHelper<'a> {
    fn new(table_name: String, rows: Rows, partition_rule: &'a PartitionRuleRef) -> Self {
        let col_name_to_idx = rows
            .schema
            .iter()
            .enumerate()
            .map(|(idx, col)| (&col.column_name, idx))
            .collect::<HashMap<_, _>>();
        let partition_cols = partition_rule.partition_columns();
        let partition_cols_indexes = partition_cols
            .into_iter()
            .map(|col_name| col_name_to_idx.get(&col_name).cloned())
            .collect::<Vec<_>>();

        Self {
            table_name,
            schema: rows.schema,
            rows: rows.rows,
            partition_rule,
            partition_cols_indexes,
        }
    }

    fn split_to_requests(mut self) -> Result<RowInsertRequestSplits> {
        let request_splits = self
            .split_to_regions()?
            .into_iter()
            .map(|(region_number, row_indexes)| {
                let rows = row_indexes
                    .into_iter()
                    .map(|row_idx| std::mem::take(&mut self.rows[row_idx]))
                    .collect();
                let req = RowInsertRequest {
                    table_name: self.table_name.clone(),
                    rows: Some(Rows {
                        schema: self.schema.clone(),
                        rows,
                    }),
                    region_number,
                };
                (region_number, req)
            })
            .collect::<HashMap<_, _>>();

        Ok(request_splits)
    }

    fn split_to_regions(&self) -> Result<HashMap<RegionNumber, Vec<usize>>> {
        let mut regions_row_indexes: HashMap<RegionNumber, Vec<usize>> = HashMap::new();
        for (row_idx, values) in self.iter_partition_values().enumerate() {
            let region_number = self.partition_rule.find_region(&values)?;
            regions_row_indexes
                .entry(region_number)
                .or_default()
                .push(row_idx);
        }

        Ok(regions_row_indexes)
    }

    fn iter_partition_values(&'a self) -> impl Iterator<Item = Vec<Value>> + 'a {
        self.rows.iter().map(|row| {
            self.partition_cols_indexes
                .iter()
                .map(|idx| {
                    idx.as_ref().map_or(Value::Null, |idx| {
                        helper::pb_value_to_value_ref(&row.values[*idx]).into()
                    })
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::Arc;

    use api::v1::value::ValueData;
    use api::v1::{ColumnDataType, SemanticType};
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::partition::PartitionExpr;
    use crate::PartitionRule;

    fn mock_insert_request() -> RowInsertRequest {
        let schema = vec![
            ColumnSchema {
                column_name: "id".to_string(),
                datatype: ColumnDataType::String as i32,
                semantic_type: SemanticType::Tag as i32,
            },
            ColumnSchema {
                column_name: "name".to_string(),
                datatype: ColumnDataType::String as i32,
                semantic_type: SemanticType::Tag as i32,
            },
            ColumnSchema {
                column_name: "age".to_string(),
                datatype: ColumnDataType::Uint32 as i32,
                semantic_type: SemanticType::Field as i32,
            },
        ];
        let rows = vec![
            Row {
                values: vec![
                    ValueData::StringValue("1".to_string()).into(),
                    ValueData::StringValue("Smith".to_string()).into(),
                    ValueData::U32Value(20).into(),
                ],
            },
            Row {
                values: vec![
                    ValueData::StringValue("2".to_string()).into(),
                    ValueData::StringValue("Johnson".to_string()).into(),
                    ValueData::U32Value(21).into(),
                ],
            },
            Row {
                values: vec![
                    ValueData::StringValue("3".to_string()).into(),
                    ValueData::StringValue("Williams".to_string()).into(),
                    ValueData::U32Value(22).into(),
                ],
            },
        ];
        RowInsertRequest {
            table_name: "t".to_string(),
            rows: Some(Rows { schema, rows }),
            region_number: 0,
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct MockPartitionRule;

    impl PartitionRule for MockPartitionRule {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn partition_columns(&self) -> Vec<String> {
            vec!["id".to_string()]
        }

        fn find_region(&self, values: &[Value]) -> Result<RegionNumber> {
            let val = values.get(0).unwrap().clone();
            let val = match val {
                Value::String(v) => v.as_utf8().to_string(),
                _ => unreachable!(),
            };

            Ok(val.parse::<u32>().unwrap() % 2)
        }

        fn find_regions_by_exprs(&self, _: &[PartitionExpr]) -> Result<Vec<RegionNumber>> {
            unimplemented!()
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct MockMissedColPartitionRule;

    impl PartitionRule for MockMissedColPartitionRule {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn partition_columns(&self) -> Vec<String> {
            vec!["missed_col".to_string()]
        }

        fn find_region(&self, values: &[Value]) -> Result<RegionNumber> {
            let val = values.get(0).unwrap().clone();
            let val = match val {
                Value::Null => 1,
                _ => 0,
            };

            Ok(val)
        }

        fn find_regions_by_exprs(&self, _: &[PartitionExpr]) -> Result<Vec<RegionNumber>> {
            unimplemented!()
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct EmptyPartitionRule;

    impl PartitionRule for EmptyPartitionRule {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn partition_columns(&self) -> Vec<String> {
            vec![]
        }

        fn find_region(&self, _values: &[Value]) -> Result<RegionNumber> {
            Ok(0)
        }

        fn find_regions_by_exprs(&self, _: &[PartitionExpr]) -> Result<Vec<RegionNumber>> {
            unimplemented!()
        }
    }

    #[test]
    fn test_writer_splitter() {
        let insert_request = mock_insert_request();
        let rule = Arc::new(MockPartitionRule) as PartitionRuleRef;
        let splitter = RowSplitter::new(rule);
        let splits = splitter.split(insert_request).unwrap();

        assert_eq!(splits.len(), 2);

        let req0 = &splits[&0];
        let req1 = &splits[&1];
        assert_eq!(req0.region_number, 0);
        assert_eq!(req1.region_number, 1);

        let rows0 = req0.rows.as_ref().unwrap();
        let rows1 = req1.rows.as_ref().unwrap();
        assert_eq!(rows0.rows.len(), 1);
        assert_eq!(rows1.rows.len(), 2);
    }

    #[test]
    fn test_missed_col_writer_splitter() {
        let insert_request = mock_insert_request();
        let rule = Arc::new(MockMissedColPartitionRule) as PartitionRuleRef;
        let splitter = RowSplitter::new(rule);
        let splits = splitter.split(insert_request).unwrap();

        assert_eq!(splits.len(), 1);

        let req = &splits[&1];
        assert_eq!(req.region_number, 1);

        let rows = req.rows.as_ref().unwrap();
        assert_eq!(rows.rows.len(), 3);
    }

    #[test]
    fn test_empty_partition_rule_writer_splitter() {
        let insert_request = mock_insert_request();
        let rule = Arc::new(EmptyPartitionRule) as PartitionRuleRef;
        let splitter = RowSplitter::new(rule);
        let splits = splitter.split(insert_request).unwrap();

        assert_eq!(splits.len(), 1);

        let req = &splits[&0];
        assert_eq!(req.region_number, 0);

        let rows = req.rows.as_ref().unwrap();
        assert_eq!(rows.rows.len(), 3);
    }
}
