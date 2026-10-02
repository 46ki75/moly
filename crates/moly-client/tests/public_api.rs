//! Consumer-side conformance: only the SDK's public facade is available here.
use moly_client::protocol;
#[path = "../../../conformance/support/server_process.rs"]
mod server_process;
#[path = "../../../conformance/sdk/public_api.rs"]
mod suite;
