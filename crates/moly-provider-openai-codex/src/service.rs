//! Provider v2 dispatch and operation-specific host capabilities.

use std::time::Duration;

use moly_protocol::auth::{AuthOperation, AuthStatus, ProviderAuthRequest};
use moly_protocol::model::{ModelRequest, PROVIDER_VERSION, ProviderInitialized};
use moly_protocol::{Body, Message, ProtocolError};
use serde::Deserialize;
use serde_json::Value;

use crate::config::Options;
use crate::error::Error;
use crate::oauth::{Credential, OAuth, now};
use crate::responses;
use crate::transport::Host;

pub(crate) struct Service {
    initialized: bool,
    oauth: OAuth,
}

#[derive(Deserialize)]
struct Initialize {
    protocol_version: u16,
}

impl Service {
    pub(crate) fn new() -> Result<Self, Error> {
        Ok(Self {
            initialized: false,
            oauth: OAuth::new()?,
        })
    }

    pub(crate) async fn handle(&mut self, request: Message, host: Host) -> Message {
        let Body::Request { id, method, params } = request.body else {
            return Message::new(Body::Error {
                id: None,
                error: Error::Host.protocol(),
            });
        };
        let result = self.dispatch(&method, params, &host).await;
        Message::new(match result {
            Ok(result) => Body::Response { id, result },
            Err(error) => Body::Error {
                id: Some(id),
                error,
            },
        })
    }

    async fn dispatch(
        &mut self,
        method: &str,
        params: Value,
        host: &Host,
    ) -> Result<Value, ProtocolError> {
        if method == "initialize" {
            if self.initialized {
                return Err(ProtocolError::new(
                    "already_initialized",
                    "Provider is already initialized",
                ));
            }
            if !params.is_object() {
                return Err(Error::InvalidParams.protocol());
            }
            let request: Initialize =
                serde_json::from_value(params).map_err(|_| Error::InvalidParams.protocol())?;
            if request.protocol_version != PROVIDER_VERSION {
                return Err(ProtocolError::new(
                    "incompatible_version",
                    "Unsupported Model Provider version",
                ));
            }
            self.initialized = true;
            return serde_json::to_value(ProviderInitialized {
                role: "model_provider".into(),
                protocol_version: PROVIDER_VERSION,
            })
            .map_err(|_| Error::Internal.protocol());
        }
        if !self.initialized {
            return Err(ProtocolError::new(
                "not_initialized",
                "Initialize the provider first",
            ));
        }
        let result = match method {
            "provider.validate" => Options::parse(params).map(|_| Value::Null),
            "provider.auth" => {
                if !params.is_object() {
                    return Err(Error::InvalidParams.protocol());
                }
                let request: ProviderAuthRequest =
                    serde_json::from_value(params).map_err(|_| Error::InvalidParams.protocol())?;
                let deadline = match request.operation {
                    AuthOperation::Login => 295,
                    _ => 27,
                };
                tokio::time::timeout(
                    Duration::from_secs(deadline),
                    self.authenticate(request, host),
                )
                .await
                .map_err(|_| Error::Timeout)
                .and_then(|result| result)
            }
            "provider.step" => {
                if !params.is_object() {
                    return Err(Error::InvalidParams.protocol());
                }
                let request: ModelRequest =
                    serde_json::from_value(params).map_err(|_| Error::InvalidParams.protocol())?;
                tokio::time::timeout(Duration::from_secs(60), self.step(request, host))
                    .await
                    .map_err(|_| Error::Timeout)
                    .and_then(|result| result)
            }
            _ => {
                return Err(ProtocolError::new(
                    "unknown_method",
                    "Unknown provider method",
                ));
            }
        };
        result.map_err(Error::protocol)
    }

    async fn authenticate(
        &self,
        request: ProviderAuthRequest,
        host: &Host,
    ) -> Result<Value, Error> {
        let options = Options::parse(request.options)?;
        // Logout always clears the selected slot, even if an old opaque record is
        // malformed. Validation/status never get a mutable credential capability.
        let loaded = Credential::load(request.credential.as_deref(), &options);
        let status = match request.operation {
            AuthOperation::Status => {
                let current = loaded?;
                let registration = current
                    .as_ref()
                    .map(|value| &value.registration)
                    .or(options.registration.as_ref());
                AuthStatus {
                    attempt_id: request.attempt_id,
                    authenticated: current
                        .as_ref()
                        .is_some_and(|value| now().is_ok_and(|now| value.usable(now))),
                    registration: registration
                        .map(serde_json::to_value)
                        .transpose()
                        .map_err(|_| Error::Internal)?,
                    revocation_confirmed: None,
                }
            }
            AuthOperation::Login => {
                let current = loaded?;
                let replacement = self
                    .oauth
                    .login(&options, current.as_ref(), request.attempt_id, host)
                    .await?;
                let registration =
                    serde_json::to_value(&replacement.registration).map_err(|_| Error::Internal)?;
                host.replace(Some(replacement.serialize()?)).await?;
                AuthStatus {
                    attempt_id: request.attempt_id,
                    authenticated: true,
                    registration: Some(registration),
                    revocation_confirmed: None,
                }
            }
            AuthOperation::Logout => {
                let current = loaded.ok().flatten();
                let confirmed = match &current {
                    Some(value) => self.oauth.revoke(value).await,
                    None => false,
                };
                host.replace(None).await?;
                let registration = current
                    .as_ref()
                    .map(|value| &value.registration)
                    .or(options.registration.as_ref());
                AuthStatus {
                    attempt_id: request.attempt_id,
                    authenticated: false,
                    registration: registration
                        .map(serde_json::to_value)
                        .transpose()
                        .map_err(|_| Error::Internal)?,
                    revocation_confirmed: Some(confirmed),
                }
            }
        };
        serde_json::to_value(status).map_err(|_| Error::Internal)
    }

    async fn step(&self, request: ModelRequest, host: &Host) -> Result<Value, Error> {
        let options = Options::parse(request.options.clone())?;
        // Reject unsupported history/tools before any token rotation or inference.
        responses::encode(&request, &options.model)?;
        let mut credential = Credential::load(request.credential.as_deref(), &options)?
            .ok_or(Error::AuthRequired)?;
        let time = now()?;
        if !credential.usable(time) {
            host.replace(None).await?;
            return Err(Error::AuthRequired);
        }
        if OAuth::needs_refresh(&credential, time) {
            credential = match self.oauth.refresh(&credential).await {
                Ok(replacement) => replacement,
                Err(Error::AuthRequired) => {
                    host.replace(None).await?;
                    return Err(Error::AuthRequired);
                }
                Err(error) => return Err(error),
            };
            host.replace(Some(credential.serialize()?)).await?;
        }
        if !credential.access_valid(now()?) {
            return Err(Error::AuthRequired);
        }
        let step = responses::infer(
            &self.oauth.client,
            &self.oauth.endpoints.responses,
            &credential.access_token,
            &request,
            &options.model,
        )
        .await?;
        serde_json::to_value(step).map_err(|_| Error::Internal)
    }

    #[cfg(test)]
    pub(crate) fn with_oauth(oauth: OAuth) -> Self {
        Self {
            initialized: false,
            oauth,
        }
    }
}
