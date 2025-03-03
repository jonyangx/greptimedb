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

//! Handling create request.

use std::sync::Arc;

use common_telemetry::info;
use snafu::{ensure, ResultExt};
use store_api::metadata::RegionMetadataBuilder;
use store_api::region_request::RegionCreateRequest;
use store_api::storage::RegionId;

use crate::error::{InvalidMetadataSnafu, RegionExistsSnafu, Result};
use crate::region::opener::RegionOpener;
use crate::worker::RegionWorkerLoop;

impl<S> RegionWorkerLoop<S> {
    pub(crate) async fn handle_create_request(
        &mut self,
        region_id: RegionId,
        request: RegionCreateRequest,
    ) -> Result<()> {
        // Checks whether the table exists.
        if self.regions.is_region_exists(region_id) {
            ensure!(
                request.create_if_not_exists,
                RegionExistsSnafu { region_id }
            );

            // Region already exists.
            return Ok(());
        }

        // Convert the request into a RegionMetadata and validate it.
        let mut builder = RegionMetadataBuilder::new(region_id);
        for column in request.column_metadatas {
            builder.push_column_metadata(column);
        }
        builder.primary_key(request.primary_key);
        let metadata = builder.build().context(InvalidMetadataSnafu)?;

        // Create a MitoRegion from the RegionMetadata.
        let region = RegionOpener::new(
            region_id,
            self.memtable_builder.clone(),
            self.object_store.clone(),
        )
        .metadata(metadata)
        .region_dir(&request.region_dir)
        .create(&self.config)
        .await?;

        // TODO(yingwen): Custom the Debug format for the metadata and also print it.
        info!("A new region created, region_id: {}", region.region_id);

        // TODO(yingwen): Metrics.

        // Insert the MitoRegion into the RegionMap.
        self.regions.insert_region(Arc::new(region));

        Ok(())
    }
}
