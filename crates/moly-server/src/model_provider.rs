//! Server policy adapter over the shared MPP host SDK.
//! Core owns configuration snapshots, credential leases, and cancellation fences;
//! this adapter binds a login's presentation callback to its initiating connection.
use crate::core::Connection;
use moly_protocol::{ProtocolError, auth::*, model::*};
use moly_provider_client::{Interaction, ProviderClient};

/// Server integration, without Provider framing or process machinery.
pub struct ModelProvider;
impl ModelProvider {
    /// Offline validation with no credential or interaction authority.
    pub async fn validate(&self, config: &ProviderConfig) -> Result<(), ProtocolError> {
        ProviderClient.validate(config).await
    }

    /// Pin presentation to the initiating Client; Core retains the credential lease.
    pub async fn authenticate(
        &self,
        config: &ProviderConfig,
        request: ProviderAuthRequest,
        credential: &mut Option<String>,
        connection: &Connection,
    ) -> Result<AuthStatus, ProtocolError> {
        let connection = connection.clone();
        let interaction = Interaction::new(move |request| {
            let connection = connection.clone();
            async move {
                let attempt = request.attempt_id;
                let response = connection.interact(request).await?;
                if response.attempt_id != attempt {
                    return Err(ProtocolError::new(
                        "provider_protocol",
                        "Invalid interaction result",
                    ));
                }
                Ok(response.outcome)
            }
        });
        ProviderClient
            .authenticate(config, request, credential, Some(interaction))
            .await
    }

    /// Execute one step; tool authority and full-lease deadlines remain in Core.
    pub async fn step(
        &self,
        config: &ProviderConfig,
        request: ModelRequest,
        credential: Option<&mut Option<String>>,
    ) -> Result<ModelStep, ProtocolError> {
        ProviderClient.step(config, request, credential).await
    }
}
