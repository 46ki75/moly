#![doc = include_str!("../README.md")]

mod client;
#[cfg(test)]
mod tests;
mod transport;

pub use client::{Client, Error, Events, Tool};
/// Shared identities, configuration, events, and protocol error types.
///
/// Re-exported so consumers need not add a separate schema dependency.
pub use moly_protocol as protocol;
