#![doc = include_str!("../README.md")]
mod client;
mod transport;

pub use client::ProviderClient;
/// Language-neutral schemas, not SDK transport or implementation details.
pub use moly_protocol as protocol;

use protocol::{
    ProtocolError,
    auth::{InteractionOutcome, InteractionRequest},
};
use std::{future::Future, pin::Pin, sync::Arc};

type Reply = Pin<Box<dyn Future<Output = Result<InteractionOutcome, ProtocolError>> + Send>>;

/// Operation-scoped host presentation callback. Presentation is not authentication.
///
/// The SDK calls it only for the active login attempt and supplies the response
/// correlation. It runs inline: cancellation drops the pending callback future.
/// There is no global registry or detached task. Never log authorization URLs.
#[derive(Clone)]
pub struct Interaction(Arc<dyn Fn(InteractionRequest) -> Reply + Send + Sync>);
impl Interaction {
    /// Adapt an async host callback. Errors are sanitized before crossing MPP:
    /// `provider_protocol` stays a protocol error; other errors mean unavailable.
    pub fn new<F, Fut>(callback: F) -> Self
    where
        F: Fn(InteractionRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<InteractionOutcome, ProtocolError>> + Send + 'static,
    {
        Self(Arc::new(move |request| Box::pin(callback(request))))
    }

    async fn present(
        &self,
        request: InteractionRequest,
    ) -> Result<InteractionOutcome, ProtocolError> {
        (self.0)(request).await.map_err(|error| {
            if error.code == "provider_protocol" {
                ProtocolError::new("provider_protocol", "Invalid host interaction result")
            } else {
                ProtocolError::new("interaction_unavailable", "Host interaction unavailable")
            }
        })
    }
}

#[cfg(test)]
#[path = "../../../conformance/protocol/framing.rs"]
mod framing;
