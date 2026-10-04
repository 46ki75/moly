//! Model Provider v2 handshake and correlated method dispatch.

use moly_protocol::model::{ModelRequest, PROVIDER_VERSION, ProviderInitialized};
use moly_protocol::{Body, Message, ProtocolError};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{self, HttpProvider};

/// One initialized-or-uninitialized peer, with one operation executing at a time.
pub(crate) struct Service {
    initialized: bool,
    provider: HttpProvider,
}

#[derive(Deserialize)]
struct Initialize {
    protocol_version: u16,
}

impl Service {
    /// Create a fresh peer state and HTTP client without discovering configuration.
    pub(crate) fn new() -> Result<Self, ProtocolError> {
        Ok(Self {
            initialized: false,
            provider: HttpProvider::new()?,
        })
    }

    /// Answer a single request; errors contain no submitted parameters.
    pub(crate) async fn handle(&mut self, message: Message) -> Message {
        let Body::Request { id, method, params } = message.body else {
            return Message::new(Body::Error {
                id: None,
                error: ProtocolError::new("invalid_frame", "Expected a request"),
            });
        };
        Message::new(match self.dispatch(&method, params).await {
            Ok(result) => Body::Response { id, result },
            Err(error) => Body::Error {
                id: Some(id),
                error,
            },
        })
    }

    async fn dispatch(&mut self, method: &str, params: Value) -> Result<Value, ProtocolError> {
        if method == "initialize" {
            if self.initialized {
                return Err(ProtocolError::new(
                    "already_initialized",
                    "Provider is already initialized",
                ));
            }
            if !params.is_object() {
                return Err(invalid_params());
            }
            let params: Initialize =
                serde_json::from_value(params).map_err(|_| invalid_params())?;
            if params.protocol_version != PROVIDER_VERSION {
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
            .map_err(|_| internal_error());
        }
        if !self.initialized {
            return Err(ProtocolError::new(
                "not_initialized",
                "Initialize the provider first",
            ));
        }
        match method {
            "provider.validate" => {
                provider::validate(params)?;
                Ok(Value::Null)
            }
            "provider.auth" => Err(ProtocolError::new(
                "auth_unsupported",
                "This Provider accepts externally supplied API credentials",
            )),
            "provider.step" => {
                if !params.is_object() {
                    return Err(invalid_params());
                }
                let request: ModelRequest =
                    serde_json::from_value(params).map_err(|_| invalid_params())?;
                serde_json::to_value(self.provider.step(request).await?)
                    .map_err(|_| internal_error())
            }
            _ => Err(ProtocolError::new(
                "unknown_method",
                "Unknown provider method",
            )),
        }
    }
}

fn invalid_params() -> ProtocolError {
    ProtocolError::new("invalid_params", "Invalid provider request parameters")
}

fn internal_error() -> ProtocolError {
    ProtocolError::new("provider_error", "Could not encode provider response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    async fn request(service: &mut Service, id: u64, method: &str, params: Value) -> Body {
        let response = service
            .handle(Message::new(Body::Request {
                id,
                method: method.into(),
                params,
            }))
            .await;
        assert_eq!(response.version, moly_protocol::VERSION);
        match &response.body {
            Body::Response { id: actual, .. }
            | Body::Error {
                id: Some(actual), ..
            } => assert_eq!(*actual, id),
            _ => panic!("expected a correlated response"),
        }
        response.body
    }

    fn assert_error(body: Body, code: &str) {
        assert!(matches!(body, Body::Error { error, .. } if error.code == code));
    }

    #[tokio::test]
    async fn handshake_requires_compatible_version_and_rejects_duplicates() -> TestResult {
        let mut service = Service::new()?;
        assert_error(
            request(&mut service, 1, "provider.validate", Value::Null).await,
            "not_initialized",
        );
        assert_error(
            request(
                &mut service,
                2,
                "initialize",
                json!({"protocol_version": 1}),
            )
            .await,
            "incompatible_version",
        );
        assert_error(
            request(&mut service, 3, "provider.step", Value::Null).await,
            "not_initialized",
        );
        assert_error(
            request(
                &mut service,
                4,
                "initialize",
                json!({"private": "sentinel"}),
            )
            .await,
            "invalid_params",
        );
        let initialized = request(
            &mut service,
            5,
            "initialize",
            json!({"protocol_version": PROVIDER_VERSION, "future": true}),
        )
        .await;
        assert!(
            matches!(initialized, Body::Response { result, .. } if result == json!({"role": "model_provider", "protocol_version": PROVIDER_VERSION}))
        );
        assert_error(
            request(
                &mut service,
                6,
                "initialize",
                json!({"protocol_version": PROVIDER_VERSION}),
            )
            .await,
            "already_initialized",
        );
        assert_error(
            request(
                &mut service,
                7,
                "future.method",
                json!({"private": "sentinel"}),
            )
            .await,
            "unknown_method",
        );
        Ok(())
    }

    #[tokio::test]
    async fn positional_arrays_are_not_request_objects() -> TestResult {
        let mut service = Service::new()?;
        assert_error(
            request(&mut service, 1, "initialize", json!([PROVIDER_VERSION])).await,
            "invalid_params",
        );
        request(
            &mut service,
            2,
            "initialize",
            json!({"protocol_version": PROVIDER_VERSION}),
        )
        .await;
        let positional = json!([
            {"model_endpoint": "not a URL", "model": "mock"},
            null,
            {
                "session_id": "00000000-0000-4000-8000-000000000001",
                "run_id": "00000000-0000-4000-8000-000000000002",
                "model_call_id": "00000000-0000-4000-8000-000000000003",
                "call_kind": "primary"
            },
            [],
            []
        ]);
        assert_error(
            request(&mut service, 3, "provider.step", positional).await,
            "invalid_params",
        );
        Ok(())
    }

    #[tokio::test]
    async fn validates_options_and_redacts_invalid_step_parameters() -> TestResult {
        let mut service = Service::new()?;
        request(
            &mut service,
            1,
            "initialize",
            json!({"protocol_version": PROVIDER_VERSION}),
        )
        .await;
        let options = json!({"model_endpoint": "https://example.invalid/exact?query", "model": "mock", "future": true});
        assert!(matches!(
            request(&mut service, 2, "provider.validate", options).await,
            Body::Response {
                result: Value::Null,
                ..
            }
        ));
        assert_error(
            request(&mut service, 3, "provider.validate", json!({})).await,
            "invalid_config",
        );
        let error = request(
            &mut service,
            4,
            "provider.step",
            json!({"credential": "private-sentinel", "messages": "private-args"}),
        )
        .await;
        let encoded = serde_json::to_string(&error)?;
        assert!(!encoded.contains("private-sentinel"));
        assert!(!encoded.contains("private-args"));
        assert_error(error, "invalid_params");
        Ok(())
    }
}
