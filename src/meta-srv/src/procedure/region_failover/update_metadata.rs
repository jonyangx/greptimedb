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

use async_trait::async_trait;
use common_meta::key::table_route::NextTableRouteKey;
use common_meta::peer::Peer;
use common_meta::rpc::router::RegionRoute;
use common_meta::RegionIdent;
use common_telemetry::info;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};

use super::invalidate_cache::InvalidateCache;
use super::{RegionFailoverContext, State};
use crate::error::{self, Result, RetryLaterSnafu};
use crate::lock::keys::table_metadata_lock_key;
use crate::lock::Opts;

#[derive(Serialize, Deserialize, Debug)]
pub(super) struct UpdateRegionMetadata {
    candidate: Peer,
}

impl UpdateRegionMetadata {
    pub(super) fn new(candidate: Peer) -> Self {
        Self { candidate }
    }

    /// Updates the metadata of the table.
    async fn update_metadata(
        &self,
        ctx: &RegionFailoverContext,
        failed_region: &RegionIdent,
    ) -> Result<()> {
        let key = table_metadata_lock_key(failed_region);
        let key = ctx.dist_lock.lock(key, Opts::default()).await?;

        self.update_table_route(ctx, failed_region).await?;

        ctx.dist_lock.unlock(key).await?;
        Ok(())
    }

    async fn update_table_route(
        &self,
        ctx: &RegionFailoverContext,
        failed_region: &RegionIdent,
    ) -> Result<()> {
        let table_id = failed_region.table_ident.table_id;

        let table_route_value = ctx
            .table_metadata_manager
            .table_route_manager()
            .get(table_id)
            .await
            .context(error::TableMetadataManagerSnafu)?
            .with_context(|| error::TableRouteNotFoundSnafu {
                table_name: failed_region.table_ident.table_ref().to_string(),
            })?;

        let mut new_region_routes = table_route_value.region_routes.clone();

        for region_route in new_region_routes.iter_mut() {
            if region_route.region.id == failed_region.region_number as u64 {
                region_route.leader_peer = Some(self.candidate.clone());
                break;
            }
        }

        pretty_log_table_route_change(
            NextTableRouteKey::new(table_id).to_string(),
            &new_region_routes,
            failed_region,
        );

        ctx.table_metadata_manager
            .update_table_route(table_id, table_route_value, new_region_routes)
            .await
            .context(error::UpdateTableRouteSnafu)?;

        Ok(())
    }
}

fn pretty_log_table_route_change(
    key: String,
    region_routes: &[RegionRoute],
    failed_region: &RegionIdent,
) {
    let region_routes = region_routes
        .iter()
        .map(|x| {
            format!(
                "{{region: {}, leader: {}, followers: [{}]}}",
                x.region.id,
                x.leader_peer
                    .as_ref()
                    .map(|p| p.id.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                x.follower_peers
                    .iter()
                    .map(|p| p.id.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            )
        })
        .collect::<Vec<_>>();

    info!(
        "Updating region routes in table route value (key = '{}') to [{}]. \
        Failed region {} was on Datanode {}.",
        key,
        region_routes.join(", "),
        failed_region.region_number,
        failed_region.datanode_id,
    );
}

#[async_trait]
#[typetag::serde]
impl State for UpdateRegionMetadata {
    async fn next(
        mut self: Box<Self>,
        ctx: &RegionFailoverContext,
        failed_region: &RegionIdent,
    ) -> Result<Box<dyn State>> {
        self.update_metadata(ctx, failed_region)
            .await
            .map_err(|e| {
                RetryLaterSnafu {
                    reason: format!(
                        "Failed to update metadata for failed region: {}, error: {}",
                        failed_region, e
                    ),
                }
                .build()
            })?;
        Ok(Box::new(InvalidateCache))
    }
}

#[cfg(test)]
mod tests {

    use common_meta::rpc::router::{extract_all_peers, region_distribution};
    use futures::TryStreamExt;

    use super::super::tests::{TestingEnv, TestingEnvBuilder};
    use super::{State, *};
    use crate::table_routes::tests::new_region_route;

    #[tokio::test]
    async fn test_next_state() {
        let env = TestingEnvBuilder::new().build().await;
        let failed_region = env.failed_region(1).await;

        let state = UpdateRegionMetadata::new(Peer::new(2, ""));

        let next_state = Box::new(state)
            .next(&env.context, &failed_region)
            .await
            .unwrap();
        assert_eq!(format!("{next_state:?}"), "InvalidateCache");
    }

    #[tokio::test]
    async fn test_update_table_route() {
        common_telemetry::init_default_ut_logging();

        async fn test(env: TestingEnv, failed_region: u32, candidate: u64) -> Vec<RegionRoute> {
            let failed_region = env.failed_region(failed_region).await;

            let state = UpdateRegionMetadata::new(Peer::new(candidate, ""));
            state
                .update_table_route(&env.context, &failed_region)
                .await
                .unwrap();

            let table_id = failed_region.table_ident.table_id;

            env.context
                .table_metadata_manager
                .table_route_manager()
                .get(table_id)
                .await
                .unwrap()
                .unwrap()
                .region_routes
        }

        // Original region routes:
        // region number => leader node
        // 1 => 1
        // 2 => 1
        // 3 => 2
        // 4 => 3

        // Testing failed region 1 moves to Datanode 2.
        let env = TestingEnvBuilder::new().build().await;
        let actual = test(env, 1, 2).await;

        // Expected region routes:
        // region number => leader node
        // 1 => 2
        // 2 => 1
        // 3 => 2
        // 4 => 3
        let peers = &extract_all_peers(&actual);
        assert_eq!(peers.len(), 3);
        let expected = vec![
            new_region_route(1, peers, 2),
            new_region_route(2, peers, 1),
            new_region_route(3, peers, 2),
            new_region_route(4, peers, 3),
        ];
        assert_eq!(actual, expected);

        // Testing failed region 3 moves to Datanode 3.
        let env = TestingEnvBuilder::new().build().await;
        let actual = test(env, 3, 3).await;

        // Expected region routes:
        // region number => leader node
        // 1 => 1
        // 2 => 1
        // 3 => 3
        // 4 => 3
        let peers = &extract_all_peers(&actual);
        assert_eq!(peers.len(), 2);
        let expected = vec![
            new_region_route(1, peers, 1),
            new_region_route(2, peers, 1),
            new_region_route(3, peers, 3),
            new_region_route(4, peers, 3),
        ];
        assert_eq!(actual, expected);

        // Testing failed region 1 moves to a new Datanode, 4.
        let env = TestingEnvBuilder::new().build().await;
        let actual = test(env, 1, 4).await;

        // Expected region routes:
        // region number => leader node
        // 1 => 4
        // 2 => 1
        // 3 => 2
        // 4 => 3
        let peers = &extract_all_peers(&actual);
        assert_eq!(peers.len(), 4);
        let expected = vec![
            new_region_route(1, peers, 4),
            new_region_route(2, peers, 1),
            new_region_route(3, peers, 2),
            new_region_route(4, peers, 3),
        ];
        assert_eq!(actual, expected);

        // Testing failed region 3 moves to a new Datanode, 4.
        let env = TestingEnvBuilder::new().build().await;
        let actual = test(env, 3, 4).await;

        // Expected region routes:
        // region number => leader node
        // 1 => 1
        // 2 => 1
        // 3 => 4
        // 4 => 3
        let peers = &extract_all_peers(&actual);
        assert_eq!(peers.len(), 3);
        let expected = vec![
            new_region_route(1, peers, 1),
            new_region_route(2, peers, 1),
            new_region_route(3, peers, 4),
            new_region_route(4, peers, 3),
        ];
        assert_eq!(actual, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_update_metadata_concurrently() {
        common_telemetry::init_default_ut_logging();

        // Test the correctness of concurrently updating the region distribution in table region
        // value, and region routes in table route value. Region 1 moves to Datanode 2; region 2
        // moves to Datanode 3.
        //
        // Datanode => Regions
        // Before:     |  After:
        // 1 => 1, 2   |
        // 2 => 3      |  2 => 3, 1
        // 3 => 4      |  3 => 4, 2
        //
        // region number => leader node
        // Before:  |  After:
        // 1 => 1   |  1 => 2
        // 2 => 1   |  2 => 3
        // 3 => 2   |  3 => 2
        // 4 => 3   |  4 => 3
        //
        // Test case runs 10 times to enlarge the possibility of concurrent updating.
        for _ in 0..10 {
            let env = TestingEnvBuilder::new().build().await;

            let ctx_1 = env.context.clone();
            let ctx_2 = env.context.clone();

            let failed_region_1 = env.failed_region(1).await;
            let failed_region_2 = env.failed_region(2).await;

            let table_id = failed_region_1.table_ident.table_id;

            let _ = futures::future::join_all(vec![
                tokio::spawn(async move {
                    let state = UpdateRegionMetadata::new(Peer::new(2, ""));
                    state
                        .update_metadata(&ctx_1, &failed_region_1)
                        .await
                        .unwrap();
                }),
                tokio::spawn(async move {
                    let state = UpdateRegionMetadata::new(Peer::new(3, ""));
                    state
                        .update_metadata(&ctx_2, &failed_region_2)
                        .await
                        .unwrap();
                }),
            ])
            .await;

            let table_route_value = env
                .context
                .table_metadata_manager
                .table_route_manager()
                .get(table_id)
                .await
                .unwrap()
                .unwrap();

            let peers = &extract_all_peers(&table_route_value.region_routes);
            let actual = &table_route_value.region_routes;
            let expected = &vec![
                new_region_route(1, peers, 2),
                new_region_route(2, peers, 3),
                new_region_route(3, peers, 2),
                new_region_route(4, peers, 3),
            ];
            assert_eq!(peers.len(), 2);
            assert_eq!(actual, expected);

            let manager = &env.context.table_metadata_manager;
            let table_route_value = manager
                .table_route_manager()
                .get(table_id)
                .await
                .unwrap()
                .unwrap();

            let map = region_distribution(&table_route_value.region_routes).unwrap();
            assert_eq!(map.len(), 2);
            assert_eq!(map.get(&2), Some(&vec![1, 3]));
            assert_eq!(map.get(&3), Some(&vec![2, 4]));

            // test DatanodeTableValues matches the table region distribution
            let datanode_table_manager = manager.datanode_table_manager();
            let tables = datanode_table_manager
                .tables(1)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert!(tables.is_empty());

            let tables = datanode_table_manager
                .tables(2)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(tables.len(), 1);
            assert_eq!(tables[0].table_id, 1);
            assert_eq!(tables[0].regions, vec![1, 3]);

            let tables = datanode_table_manager
                .tables(3)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(tables.len(), 1);
            assert_eq!(tables[0].table_id, 1);
            assert_eq!(tables[0].regions, vec![2, 4]);
        }
    }
}
