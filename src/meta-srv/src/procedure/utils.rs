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

use common_meta::peer::Peer;
use common_procedure::error::Error as ProcedureError;
use snafu::{location, Location};

use crate::error::{self, Error};

pub fn handle_request_datanode_error(datanode: Peer) -> impl FnOnce(client::error::Error) -> Error {
    move |err| {
        if matches!(err, client::error::Error::FlightGet { .. }) {
            error::RetryLaterSnafu {
                reason: format!("Failed to execute operation on datanode, source: {}", err),
            }
            .build()
        } else {
            error::Error::RequestDatanode {
                location: location!(),
                peer: datanode,
                source: err,
            }
        }
    }
}

pub fn handle_retry_error(e: Error) -> ProcedureError {
    if matches!(e, error::Error::RetryLater { .. }) {
        ProcedureError::retry_later(e)
    } else {
        ProcedureError::external(e)
    }
}
