//! Test adapters for Server internals and their protocol contract.
#[path = "../../../conformance/state-transitions/config.rs"]
mod config;
#[path = "../../../conformance/protocol/duplex.rs"]
mod duplex;
#[path = "../../../conformance/protocol/framing.rs"]
mod framing;
#[path = "../../../conformance/protocol/local_transport.rs"]
mod local_transport;
#[path = "../../../conformance/support/provider_process.rs"]
mod provider_process;
#[path = "../../../conformance/state-transitions/slow_consumer.rs"]
mod slow_consumer;
#[path = "../../../conformance/protocol/trace.rs"]
mod trace;
