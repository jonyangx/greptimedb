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
use api::helper::ColumnDataTypeWrapper;
use api::v1::column::Values;
use api::v1::{AddColumns, Column, CreateTableExpr, InsertRequest as GrpcInsertRequest};
use common_base::BitVec;
use datatypes::data_type::{ConcreteDataType, DataType};
use datatypes::prelude::VectorRef;
use datatypes::schema::SchemaRef;
use snafu::{ensure, ResultExt};
use table::engine::TableReference;
use table::metadata::TableId;
use table::requests::InsertRequest;

use crate::error::{
    ColumnAlreadyExistsSnafu, ColumnDataTypeSnafu, CreateVectorSnafu, Result,
    UnexpectedValuesLengthSnafu,
};
use crate::util;
use crate::util::ColumnExpr;

pub fn find_new_columns(schema: &SchemaRef, columns: &[Column]) -> Result<Option<AddColumns>> {
    let column_exprs = ColumnExpr::from_columns(columns);
    util::extract_new_columns(schema, column_exprs)
}

/// Try to build create table request from insert data.
pub fn build_create_expr_from_insertion(
    catalog_name: &str,
    schema_name: &str,
    table_id: Option<TableId>,
    table_name: &str,
    columns: &[Column],
    engine: &str,
) -> Result<CreateTableExpr> {
    let table_name = TableReference::full(catalog_name, schema_name, table_name);
    let column_exprs = ColumnExpr::from_columns(columns);
    util::build_create_table_expr(
        table_id,
        &table_name,
        column_exprs,
        engine,
        "Created on insertion",
    )
}

pub fn to_table_insert_request(
    catalog_name: &str,
    schema_name: &str,
    request: GrpcInsertRequest,
) -> Result<InsertRequest> {
    let table_name = &request.table_name;
    let row_count = request.row_count as usize;

    let mut columns_values = HashMap::with_capacity(request.columns.len());
    for Column {
        column_name,
        values,
        null_mask,
        datatype,
        ..
    } in request.columns
    {
        let Some(values) = values else { continue };

        let datatype: ConcreteDataType = ColumnDataTypeWrapper::try_new(datatype)
            .context(ColumnDataTypeSnafu)?
            .into();
        let vector = add_values_to_builder(datatype, values, row_count, null_mask)?;

        ensure!(
            columns_values.insert(column_name.clone(), vector).is_none(),
            ColumnAlreadyExistsSnafu {
                column: column_name
            }
        );
    }

    Ok(InsertRequest {
        catalog_name: catalog_name.to_string(),
        schema_name: schema_name.to_string(),
        table_name: table_name.to_string(),
        columns_values,
        region_number: request.region_number,
    })
}

pub(crate) fn add_values_to_builder(
    data_type: ConcreteDataType,
    values: Values,
    row_count: usize,
    null_mask: Vec<u8>,
) -> Result<VectorRef> {
    if null_mask.is_empty() {
        Ok(helper::pb_values_to_vector_ref(&data_type, values))
    } else {
        let builder = &mut data_type.create_mutable_vector(row_count);
        let values = helper::pb_values_to_values(&data_type, values);
        let null_mask = BitVec::from_vec(null_mask);
        ensure!(
            null_mask.count_ones() + values.len() == row_count,
            UnexpectedValuesLengthSnafu {
                reason: "If null_mask is not empty, the sum of the number of nulls and the length of values must be equal to row_count."
            }
        );

        let mut idx_of_values = 0;
        for idx in 0..row_count {
            match is_null(&null_mask, idx) {
                Some(true) => builder.push_null(),
                _ => {
                    builder
                        .try_push_value_ref(values[idx_of_values].as_value_ref())
                        .context(CreateVectorSnafu)?;
                    idx_of_values += 1
                }
            }
        }
        Ok(builder.to_vector())
    }
}

fn is_null(null_mask: &BitVec, idx: usize) -> Option<bool> {
    null_mask.get(idx).as_deref().copied()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::{assert_eq, vec};

    use api::helper::ColumnDataTypeWrapper;
    use api::v1::column::Values;
    use api::v1::{Column, ColumnDataType, IntervalMonthDayNano, SemanticType};
    use common_base::BitVec;
    use common_catalog::consts::MITO_ENGINE;
    use common_time::interval::IntervalUnit;
    use common_time::timestamp::{TimeUnit, Timestamp};
    use datatypes::data_type::ConcreteDataType;
    use datatypes::schema::{ColumnSchema, SchemaBuilder};
    use datatypes::value::Value;
    use snafu::ResultExt;

    use super::*;
    use crate::error;
    use crate::error::ColumnDataTypeSnafu;
    use crate::insert::find_new_columns;

    #[inline]
    fn build_column_schema(
        column_name: &str,
        datatype: i32,
        nullable: bool,
    ) -> error::Result<ColumnSchema> {
        let datatype_wrapper =
            ColumnDataTypeWrapper::try_new(datatype).context(ColumnDataTypeSnafu)?;

        Ok(ColumnSchema::new(
            column_name,
            datatype_wrapper.into(),
            nullable,
        ))
    }

    #[test]
    fn test_build_create_table_request() {
        let table_id = Some(10);
        let table_name = "test_metric";

        assert!(
            build_create_expr_from_insertion("", "", table_id, table_name, &[], MITO_ENGINE)
                .is_err()
        );

        let insert_batch = mock_insert_batch();

        let create_expr = build_create_expr_from_insertion(
            "",
            "",
            table_id,
            table_name,
            &insert_batch.0,
            MITO_ENGINE,
        )
        .unwrap();

        assert_eq!(table_id, create_expr.table_id.map(|x| x.id));
        assert_eq!(table_name, create_expr.table_name);
        assert_eq!("Created on insertion".to_string(), create_expr.desc);
        assert_eq!(
            vec![create_expr.column_defs[0].name.clone()],
            create_expr.primary_keys
        );

        let column_defs = create_expr.column_defs;
        assert_eq!(column_defs[5].name, create_expr.time_index);
        assert_eq!(6, column_defs.len());

        assert_eq!(
            ConcreteDataType::string_datatype(),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(
                    column_defs
                        .iter()
                        .find(|c| c.name == "host")
                        .unwrap()
                        .datatype
                )
                .unwrap()
            )
        );

        assert_eq!(
            ConcreteDataType::float64_datatype(),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(
                    column_defs
                        .iter()
                        .find(|c| c.name == "cpu")
                        .unwrap()
                        .datatype
                )
                .unwrap()
            )
        );

        assert_eq!(
            ConcreteDataType::float64_datatype(),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(
                    column_defs
                        .iter()
                        .find(|c| c.name == "memory")
                        .unwrap()
                        .datatype
                )
                .unwrap()
            )
        );

        assert_eq!(
            ConcreteDataType::time_datatype(TimeUnit::Millisecond),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(
                    column_defs
                        .iter()
                        .find(|c| c.name == "time")
                        .unwrap()
                        .datatype
                )
                .unwrap()
            )
        );

        assert_eq!(
            ConcreteDataType::interval_datatype(IntervalUnit::MonthDayNano),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(
                    column_defs
                        .iter()
                        .find(|c| c.name == "interval")
                        .unwrap()
                        .datatype
                )
                .unwrap()
            )
        );

        assert_eq!(
            ConcreteDataType::timestamp_millisecond_datatype(),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(
                    column_defs
                        .iter()
                        .find(|c| c.name == "ts")
                        .unwrap()
                        .datatype
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn test_find_new_columns() {
        let mut columns = Vec::with_capacity(1);
        let cpu_column = build_column_schema("cpu", 10, true).unwrap();
        let ts_column = build_column_schema("ts", 15, false)
            .unwrap()
            .with_time_index(true);
        columns.push(cpu_column);
        columns.push(ts_column);

        let schema = Arc::new(SchemaBuilder::try_from(columns).unwrap().build().unwrap());

        assert!(find_new_columns(&schema, &[]).unwrap().is_none());

        let insert_batch = mock_insert_batch();

        let add_columns = find_new_columns(&schema, &insert_batch.0).unwrap().unwrap();

        assert_eq!(4, add_columns.add_columns.len());
        let host_column = &add_columns.add_columns[0];
        assert!(host_column.is_key);

        assert_eq!(
            ConcreteDataType::string_datatype(),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(host_column.column_def.as_ref().unwrap().datatype)
                    .unwrap()
            )
        );

        let memory_column = &add_columns.add_columns[1];
        assert!(!memory_column.is_key);

        assert_eq!(
            ConcreteDataType::float64_datatype(),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(memory_column.column_def.as_ref().unwrap().datatype)
                    .unwrap()
            )
        );

        let time_column = &add_columns.add_columns[2];
        assert!(!time_column.is_key);

        assert_eq!(
            ConcreteDataType::time_datatype(TimeUnit::Millisecond),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(time_column.column_def.as_ref().unwrap().datatype)
                    .unwrap()
            )
        );

        let interval_column = &add_columns.add_columns[3];
        assert!(!interval_column.is_key);

        assert_eq!(
            ConcreteDataType::interval_datatype(IntervalUnit::MonthDayNano),
            ConcreteDataType::from(
                ColumnDataTypeWrapper::try_new(
                    interval_column.column_def.as_ref().unwrap().datatype
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn test_to_table_insert_request() {
        let (columns, row_count) = mock_insert_batch();
        let request = GrpcInsertRequest {
            table_name: "demo".to_string(),
            columns,
            row_count,
            region_number: 0,
        };
        let insert_req = to_table_insert_request("greptime", "public", request).unwrap();

        assert_eq!("greptime", insert_req.catalog_name);
        assert_eq!("public", insert_req.schema_name);
        assert_eq!("demo", insert_req.table_name);

        let host = insert_req.columns_values.get("host").unwrap();
        assert_eq!(Value::String("host1".into()), host.get(0));
        assert_eq!(Value::String("host2".into()), host.get(1));

        let cpu = insert_req.columns_values.get("cpu").unwrap();
        assert_eq!(Value::Float64(0.31.into()), cpu.get(0));
        assert_eq!(Value::Null, cpu.get(1));

        let memory = insert_req.columns_values.get("memory").unwrap();
        assert_eq!(Value::Null, memory.get(0));
        assert_eq!(Value::Float64(0.1.into()), memory.get(1));

        let ts = insert_req.columns_values.get("ts").unwrap();
        assert_eq!(Value::Timestamp(Timestamp::new_millisecond(100)), ts.get(0));
        assert_eq!(Value::Timestamp(Timestamp::new_millisecond(101)), ts.get(1));
    }

    #[test]
    fn test_is_null() {
        let null_mask = BitVec::from_slice(&[0b0000_0001, 0b0000_1000]);

        assert_eq!(Some(true), is_null(&null_mask, 0));
        assert_eq!(Some(false), is_null(&null_mask, 1));
        assert_eq!(Some(false), is_null(&null_mask, 10));
        assert_eq!(Some(true), is_null(&null_mask, 11));
        assert_eq!(Some(false), is_null(&null_mask, 12));

        assert_eq!(None, is_null(&null_mask, 16));
        assert_eq!(None, is_null(&null_mask, 99));
    }

    fn mock_insert_batch() -> (Vec<Column>, u32) {
        let row_count = 2;

        let host_vals = Values {
            string_values: vec!["host1".to_string(), "host2".to_string()],
            ..Default::default()
        };
        let host_column = Column {
            column_name: "host".to_string(),
            semantic_type: SemanticType::Tag as i32,
            values: Some(host_vals),
            null_mask: vec![0],
            datatype: ColumnDataType::String as i32,
        };

        let cpu_vals = Values {
            f64_values: vec![0.31],
            ..Default::default()
        };
        let cpu_column = Column {
            column_name: "cpu".to_string(),
            semantic_type: SemanticType::Field as i32,
            values: Some(cpu_vals),
            null_mask: vec![2],
            datatype: ColumnDataType::Float64 as i32,
        };

        let mem_vals = Values {
            f64_values: vec![0.1],
            ..Default::default()
        };
        let mem_column = Column {
            column_name: "memory".to_string(),
            semantic_type: SemanticType::Field as i32,
            values: Some(mem_vals),
            null_mask: vec![1],
            datatype: ColumnDataType::Float64 as i32,
        };

        let time_vals = Values {
            time_millisecond_values: vec![100, 101],
            ..Default::default()
        };
        let time_column = Column {
            column_name: "time".to_string(),
            semantic_type: SemanticType::Field as i32,
            values: Some(time_vals),
            null_mask: vec![0],
            datatype: ColumnDataType::TimeMillisecond as i32,
        };

        let interval1 = IntervalMonthDayNano {
            months: 1,
            days: 2,
            nanoseconds: 3,
        };
        let interval2 = IntervalMonthDayNano {
            months: 4,
            days: 5,
            nanoseconds: 6,
        };
        let interval_vals = Values {
            interval_month_day_nano_values: vec![interval1, interval2],
            ..Default::default()
        };
        let interval_column = Column {
            column_name: "interval".to_string(),
            semantic_type: SemanticType::Field as i32,
            values: Some(interval_vals),
            null_mask: vec![0],
            datatype: ColumnDataType::IntervalMonthDayNano as i32,
        };

        let ts_vals = Values {
            ts_millisecond_values: vec![100, 101],
            ..Default::default()
        };
        let ts_column = Column {
            column_name: "ts".to_string(),
            semantic_type: SemanticType::Timestamp as i32,
            values: Some(ts_vals),
            null_mask: vec![0],
            datatype: ColumnDataType::TimestampMillisecond as i32,
        };

        (
            vec![
                host_column,
                cpu_column,
                mem_column,
                time_column,
                interval_column,
                ts_column,
            ],
            row_count,
        )
    }
}
