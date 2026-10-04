//! Conformance adapters for SDK behavior and its private transport.
#[path = "auth_tests.rs"]
mod auth;
#[path = "../../../conformance/protocol/client_lifetime.rs"]
mod client_lifetime;
#[path = "../../../conformance/protocol/duplex.rs"]
mod duplex;
#[path = "../../../conformance/state-transitions/e2e.rs"]
mod e2e;
#[path = "../../../conformance/protocol/framing.rs"]
mod framing;
#[path = "../../../conformance/protocol/local_transport.rs"]
mod local_transport;
#[path = "../../../conformance/state-transitions/process.rs"]
mod process;
#[path = "../../../conformance/support/server_process.rs"]
mod server_process;
#[path = "../../../conformance/protocol/trace.rs"]
mod trace;
