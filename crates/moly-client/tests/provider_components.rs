//! Cross-language Provider conformance through only the public Client SDK.
use moly_client::protocol;
#[path = "../../../conformance/model-provider/audited_server.rs"]
mod audited_server;
#[path = "../../../conformance/auth/suite.rs"]
mod auth;
#[path = "../../../conformance/support/server_process.rs"]
mod server_process;
#[path = "../../../conformance/model-provider/suite.rs"]
mod suite;
