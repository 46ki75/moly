//! REPL conformance through the actual CLI and Server executables.
use moly_client::protocol;

#[path = "../../../conformance/state-transitions/repl.rs"]
mod repl;
#[path = "../../../conformance/support/server_process.rs"]
mod server_process;
#[path = "../../../conformance/state-transitions/cli.rs"]
mod suite;
