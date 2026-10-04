//! Host-owned, single-invocation MPP processes. No model-specific encoding.
use crate::{
    Interaction,
    transport::{encode_frame, read_frame},
};
use moly_protocol::{AuthAttemptId, Body, Message, ProtocolError, auth::*, model::*};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(3);
const STEP_TIMEOUT: Duration = Duration::from_secs(65);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

struct Host<'a> {
    credential: Option<&'a mut Option<String>>,
    interaction: Option<(Interaction, AuthAttemptId)>,
}

/// Stateless host-side MPP client. Each call explicitly launches a fresh child.
///
/// No environment/config discovery, retries, agent state, or global credential
/// storage. Hosts must serialize access to each renewable credential for the full
/// operation. Dropping a call kills its child and drops any interaction callback;
/// acknowledged credential replacements remain in the borrowed slot.
#[derive(Clone, Copy, Default)]
pub struct ProviderClient;
impl ProviderClient {
    /// Validate options without model calls, interaction, or credential access.
    /// Handshake and validation each have a three-second deadline.
    pub async fn validate(&self, config: &ProviderConfig) -> Result<(), ProtocolError> {
        validate_command(&config.command)?;
        let mut process = Process::spawn(&config.command).await?;
        let result = process
            .request(
                2,
                "provider.validate",
                config.options.clone(),
                VALIDATION_TIMEOUT,
                Host {
                    credential: None,
                    interaction: None,
                },
            )
            .await
            .and_then(|value| {
                if value.is_null() {
                    Ok(())
                } else {
                    Err(protocol_error())
                }
            });
        process.stop().await;
        result
    }

    /// Authenticate with a selected credential slot and optional login UI.
    ///
    /// Options and credentials are taken from `config` and `credential`, replacing
    /// any duplicate values in `request`. Only login can interact; status cannot
    /// write credentials. A replacement commits before its acknowledgment and is
    /// not rolled back on cancellation or a subsequent error. Login has a total
    /// 300-second deadline, status/logout 30 seconds, including startup and callbacks.
    pub async fn authenticate(
        &self,
        config: &ProviderConfig,
        mut request: ProviderAuthRequest,
        credential: &mut Option<String>,
        interaction: Option<Interaction>,
    ) -> Result<AuthStatus, ProtocolError> {
        let attempt = request.attempt_id;
        let login = request.operation == AuthOperation::Login;
        let writable = request.operation != AuthOperation::Status;
        let deadline = if login { LOGIN_TIMEOUT } else { AUTH_TIMEOUT };
        bounded(deadline, async {
            validate_command(&config.command)?;
            request.options = config.options.clone();
            request.credential = credential.clone();
            let params = serde_json::to_value(&request).map_err(|_| protocol_error())?;
            let mut process = Process::spawn(&config.command).await?;
            let result = process
                .request(
                    2,
                    "provider.auth",
                    params,
                    deadline,
                    Host {
                        credential: writable.then_some(credential),
                        interaction: interaction.filter(|_| login).map(|ui| (ui, attempt)),
                    },
                )
                .await
                .and_then(|value| {
                    if !value.is_object() {
                        return Err(protocol_error());
                    }
                    let status: AuthStatus =
                        serde_json::from_value(value).map_err(|_| protocol_error())?;
                    if status.attempt_id != attempt
                        || status.registration.as_ref().is_some_and(|value| {
                            !value.is_object() || value.to_string().len() > 16 * 1024
                        })
                    {
                        return Err(protocol_error());
                    }
                    Ok(status)
                });
            process.stop().await;
            result
        })
        .await
    }

    /// One model step with a 65-second total deadline, including startup.
    ///
    /// `config.options` and the selected slot replace duplicate request values.
    /// `None` grants no credential read/write authority. No interaction or tool
    /// execution is performed; requested tools are validated against `request.tools`.
    /// Dropping this future kills its child, without retrying.
    #[tracing::instrument(skip_all, fields(
        session_id = %request.context.session_id,
        run_id = %request.context.run_id,
        model_call_id = %request.context.model_call_id,
    ))]
    pub async fn step(
        &self,
        config: &ProviderConfig,
        mut request: ModelRequest,
        credential: Option<&mut Option<String>>,
    ) -> Result<ModelStep, ProtocolError> {
        bounded(STEP_TIMEOUT, async {
            validate_command(&config.command)?;
            request.options = config.options.clone();
            request.credential = credential.as_deref().cloned().flatten();
            let params = serde_json::to_value(&request).map_err(|_| protocol_error())?;
            let mut process = Process::spawn(&config.command).await?;
            let result = process
                .request(
                    2,
                    "provider.step",
                    params,
                    STEP_TIMEOUT,
                    Host {
                        credential,
                        interaction: None,
                    },
                )
                .await
                .and_then(|value| decode_step(value, &request));
            process.stop().await;
            result
        })
        .await
    }
}

async fn bounded<T>(
    deadline: Duration,
    work: impl std::future::Future<Output = Result<T, ProtocolError>>,
) -> Result<T, ProtocolError> {
    tokio::time::timeout(deadline, work).await.map_err(|_| {
        ProtocolError::new(
            "provider_timeout",
            "MPP operation timed out; outcome may be uncertain",
        )
    })?
}

fn validate_command(command: &ComponentCommand) -> Result<(), ProtocolError> {
    if !Path::new(&command.executable).is_absolute()
        || command.executable.contains('\0')
        || command.executable.len() > 32 * 1024
        || command.args.len() > 128
        || command
            .args
            .iter()
            .any(|arg| arg.contains('\0') || arg.len() > 32 * 1024)
        || command.env.len() > 128
        || command.env.iter().any(|(key, value)| {
            key.is_empty()
                || key.contains(['=', '\0'])
                || key.len() > 256
                || value.contains('\0')
                || value.len() > 32 * 1024
        })
    {
        return Err(ProtocolError::new(
            "invalid_config",
            "Invalid resolved Provider launch command",
        ));
    }
    Ok(())
}

fn decode_step(value: Value, request: &ModelRequest) -> Result<ModelStep, ProtocolError> {
    // Serde structs also accept positional sequences; those are not part of this
    // JSON contract and must not authorize tool execution through incidental Rust decoding.
    if !value.is_object()
        || value
            .get("metadata")
            .is_some_and(|metadata| !metadata.is_null() && !metadata.is_object())
        || value
            .get("calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| calls.iter().any(|call| !call.is_object()))
    {
        return Err(protocol_error());
    }
    let step = serde_json::from_value(value).map_err(|_| protocol_error())?;
    validate_step(step, request)
}

fn validate_step(step: ModelStep, request: &ModelRequest) -> Result<ModelStep, ProtocolError> {
    if let ModelStep::AwaitHostTools { calls, .. } = &step {
        let mut ids = HashSet::new();
        if calls.is_empty()
            || calls.len() > 32
            || calls.iter().any(|call| {
                call.id.is_empty()
                    || !ids.insert(&call.id)
                    || !call.arguments.is_object()
                    || !request.tools.iter().any(|tool| tool.name == call.name)
            })
        {
            return Err(protocol_error());
        }
    }
    Ok(step)
}

struct Process {
    // Cancellation at any await, including handshake/write/reply, owns cleanup.
    // Tokio's kill_on_drop also arranges best-effort reaping after cancellation.
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Process {
    async fn spawn(command: &ComponentCommand) -> Result<Self, ProtocolError> {
        let mut child = Command::new(&command.executable)
            .args(&command.args)
            .env_clear()
            .envs(&command.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // An independently implemented peer may put secrets in diagnostics.
            // Never forward its untrusted stderr into host/user logs.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| {
                ProtocolError::new(
                    "provider_unavailable",
                    "Could not start configured Model Provider",
                )
            })?;
        let input = child.stdin.take().ok_or_else(protocol_error)?;
        let output = BufReader::new(child.stdout.take().ok_or_else(protocol_error)?);
        let mut process = Self {
            child,
            input,
            output,
        };
        let result = process
            .request(
                1,
                "initialize",
                json!({"protocol_version": PROVIDER_VERSION}),
                HANDSHAKE_TIMEOUT,
                Host {
                    credential: None,
                    interaction: None,
                },
            )
            .await?;
        if !result.is_object() {
            return Err(protocol_error());
        }
        let initialized: ProviderInitialized =
            serde_json::from_value(result).map_err(|_| protocol_error())?;
        if initialized.role != "model_provider" || initialized.protocol_version != PROVIDER_VERSION
        {
            return Err(protocol_error());
        }
        tracing::debug!("MPP handshake completed");
        Ok(process)
    }

    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        deadline: Duration,
        mut host: Host<'_>,
    ) -> Result<Value, ProtocolError> {
        let bytes = encode_frame(&Message::new(Body::Request {
            id,
            method: method.into(),
            params,
        }))
        .map_err(|_| {
            ProtocolError::new(
                "provider_request_too_large",
                "Provider request exceeds the protocol limit",
            )
        })?;
        tokio::time::timeout(deadline, async {
            self.input
                .write_all(&bytes)
                .await
                .map_err(|_| unavailable())?;
            self.input.flush().await.map_err(|_| unavailable())?;
            let mut last_host_id = 0;
            let mut host_calls = 0;
            loop {
                let message = read_frame(&mut self.output)
                    .await
                    .map_err(|_| protocol_error())?
                    .ok_or_else(unavailable)?;
                match message.body {
                    Body::Response {
                        id: returned,
                        result,
                    } if returned == id => return Ok(result),
                    Body::Error {
                        id: Some(returned),
                        error,
                    } if returned == id => return Err(redact(error)),
                    Body::Request {
                        id: reverse,
                        method,
                        params,
                    } if id != 1 && reverse > last_host_id && host_calls < 64 => {
                        last_host_id = reverse;
                        host_calls += 1;
                        let result = host.request(&method, params).await;
                        let response = Message::new(match result {
                            Ok(result) => Body::Response {
                                id: reverse,
                                result,
                            },
                            Err(error) => Body::Error {
                                id: Some(reverse),
                                error,
                            },
                        });
                        let bytes = encode_frame(&response).map_err(|_| protocol_error())?;
                        self.input
                            .write_all(&bytes)
                            .await
                            .map_err(|_| unavailable())?;
                        self.input.flush().await.map_err(|_| unavailable())?;
                    }
                    _ => return Err(protocol_error()),
                }
            }
        })
        .await
        .map_err(|_| ProtocolError::new("provider_timeout", "Model Provider operation timed out"))?
    }

    async fn stop(&mut self) {
        // No process reuse or implicit replay in this slice. Successful replies
        // finish this invocation; the process cannot keep working in the background.
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
        tracing::debug!("MPP process cleanup attempted");
    }
}

impl Host<'_> {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ProtocolError> {
        if !params.is_object() {
            return Err(protocol_error());
        }
        match method {
            "host.credential.replace" => {
                let slot = self.credential.as_mut().ok_or_else(|| {
                    ProtocolError::new("host_service_unavailable", "No writable credential scope")
                })?;
                if params.get("credential").is_none() {
                    return Err(protocol_error());
                }
                let replace: CredentialReplace =
                    serde_json::from_value(params).map_err(|_| protocol_error())?;
                if replace
                    .credential
                    .as_ref()
                    .is_some_and(|value| value.len() > 64 * 1024)
                {
                    return Err(ProtocolError::new(
                        "invalid_params",
                        "Credential exceeds size limit",
                    ));
                }
                **slot = replace.credential;
                Ok(Value::Null)
            }
            "host.interact" => {
                let (interaction, attempt) = self.interaction.as_ref().ok_or_else(|| {
                    ProtocolError::new(
                        "interaction_unavailable",
                        "No interactive authentication operation",
                    )
                })?;
                let request: InteractionRequest =
                    serde_json::from_value(params).map_err(|_| protocol_error())?;
                if request.attempt_id != *attempt
                    || !request.url.starts_with("https://")
                    || request.url.len() > 8192
                    || request.url.chars().any(char::is_control)
                {
                    return Err(protocol_error());
                }
                let outcome = interaction.present(request).await?;
                serde_json::to_value(InteractionResponse {
                    attempt_id: *attempt,
                    outcome,
                })
                .map_err(|_| protocol_error())
            }
            _ => Err(ProtocolError::new(
                "unknown_method",
                "Unknown Provider host service",
            )),
        }
    }
}

fn protocol_error() -> ProtocolError {
    ProtocolError::new(
        "provider_protocol",
        "Invalid Model Provider protocol response",
    )
}
fn unavailable() -> ProtocolError {
    ProtocolError::new(
        "provider_unavailable",
        "Model Provider connection closed; operation outcome may be uncertain",
    )
}
fn redact(error: ProtocolError) -> ProtocolError {
    // Codes are also untrusted strings. Never echo arbitrary peer text, even when
    // it arrived in a well-formed structured error instead of an HTTP body.
    let (code, message) = match error.code.as_str() {
        "invalid_config" => (
            "invalid_config",
            "Model Provider rejected its configuration",
        ),
        "invalid_params" => (
            "invalid_params",
            "Model Provider rejected the request parameters",
        ),
        "invalid_secret" => ("invalid_secret", "Model Provider rejected its credential"),
        "auth_required" => ("auth_required", "Provider authentication is required"),
        "auth_failed" => ("auth_failed", "Provider authentication failed"),
        "auth_declined" => ("auth_declined", "Authentication was declined"),
        "auth_cancelled" => ("auth_cancelled", "Authentication was cancelled"),
        "interaction_unavailable" | "auth_interaction_unavailable" => (
            "interaction_unavailable",
            "No interactive host is available",
        ),
        "auth_scope_required" => (
            "auth_scope_required",
            "Required authentication permission was not granted",
        ),
        "auth_invalid_client" => (
            "auth_invalid_client",
            "Provider authentication registration is invalid",
        ),
        "provider_usage_limit" => ("provider_usage_limit", "Provider usage limit reached"),
        "provider_usage_unavailable" => (
            "provider_usage_unavailable",
            "Provider usage is temporarily unavailable",
        ),
        "provider_permission_denied" => {
            ("provider_permission_denied", "Provider access was denied")
        }
        "provider_incomplete" => ("provider_incomplete", "Model response was incomplete"),
        "provider_interrupted" => ("provider_interrupted", "Model response was interrupted"),
        "provider_replay_invalid" => (
            "provider_replay_invalid",
            "Model replay metadata is invalid",
        ),
        "provider_unavailable" => ("provider_unavailable", "Provider service is unavailable"),
        "provider_protocol" => ("provider_protocol", "Invalid Provider protocol result"),
        "auth_unsupported" | "unknown_method" => (
            "auth_unsupported",
            "Provider does not support authentication operations",
        ),
        "provider_timeout" => ("provider_timeout", "Model Provider operation timed out"),
        "provider_response_invalid" => (
            "provider_response_invalid",
            "Invalid upstream model response",
        ),
        "provider_response_too_large" => (
            "provider_response_too_large",
            "Upstream model response exceeds the limit",
        ),
        "provider_unsupported" => (
            "provider_unsupported",
            "Unsupported model feature or replay metadata",
        ),
        _ => ("provider_error", "Model Provider operation failed"),
    };
    ProtocolError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn host_credential_writes_require_scope_and_an_explicit_value()
    -> Result<(), ProtocolError> {
        let mut readonly = Host {
            credential: None,
            interaction: None,
        };
        assert_eq!(
            readonly
                .request("host.credential.replace", json!({"credential":"sentinel"}))
                .await
                .expect_err("read-only host")
                .code,
            "host_service_unavailable"
        );
        let mut slot = Some("original".into());
        let mut host = Host {
            credential: Some(&mut slot),
            interaction: None,
        };
        for malformed in [json!({}), json!({"credential":7}), json!([])] {
            assert!(
                host.request("host.credential.replace", malformed)
                    .await
                    .is_err()
            );
        }
        assert!(
            host.request(
                "host.credential.replace",
                json!({"credential":"x".repeat(65537)})
            )
            .await
            .is_err()
        );
        assert_eq!(
            host.credential.as_deref().and_then(Option::as_deref),
            Some("original")
        );
        assert_eq!(
            host.request("host.credential.replace", json!({"credential":"rotated"}))
                .await?,
            Value::Null
        );
        assert_eq!(
            host.credential.as_deref().and_then(Option::as_deref),
            Some("rotated")
        );
        host.request("host.credential.replace", json!({"credential":null}))
            .await?;
        assert!(slot.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn interactions_are_bound_to_attempt_and_operation_callback() -> Result<(), ProtocolError>
    {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let attempt = AuthAttemptId::new();
        let interaction = Interaction::new(move |request| {
            assert_eq!(request.attempt_id, attempt);
            observed.fetch_add(1, Ordering::SeqCst);
            async { Ok(InteractionOutcome::Opened) }
        });
        let mut host = Host {
            credential: None,
            interaction: Some((interaction, attempt)),
        };
        for request in [
            json!({"attempt_id":AuthAttemptId::new(),"url":"https://example.invalid"}),
            json!({"attempt_id":attempt,"url":"file:///secret"}),
            json!({"attempt_id":attempt,"url":"https://example.invalid/\nsecret"}),
            json!({"attempt_id":attempt,"url":format!("https://{}", "a".repeat(8192))}),
        ] {
            assert!(host.request("host.interact", request).await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let result = host
            .request(
                "host.interact",
                json!({"attempt_id":attempt,"url":"https://example.invalid/authorize"}),
            )
            .await?;
        assert_eq!(result["outcome"], "opened");
        assert_eq!(result["attempt_id"], json!(attempt));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut no_ui = Host {
            credential: None,
            interaction: None,
        };
        assert_eq!(
            no_ui
                .request(
                    "host.interact",
                    json!({"attempt_id":attempt,"url":"https://example.invalid"})
                )
                .await
                .expect_err("no interactive scope")
                .code,
            "interaction_unavailable"
        );
        Ok(())
    }

    #[test]
    fn untrusted_errors_do_not_echo_codes_or_messages() {
        let error = redact(ProtocolError::new("credential-sentinel", "prompt-sentinel"));
        assert_eq!(error.code, "provider_error");
        assert!(!error.to_string().contains("sentinel"));
    }
    #[test]
    fn provider_results_require_schema_objects_not_positional_arrays() {
        use moly_protocol::{ModelCallId, RunId, SessionId, ToolDefinition};
        let request = ModelRequest {
            options: Value::Null,
            credential: None,
            context: InferenceContext {
                session_id: SessionId::new(),
                run_id: RunId::new(),
                model_call_id: ModelCallId::new(),
                call_kind: CallKind::Primary,
            },
            messages: vec![],
            tools: vec![ToolDefinition {
                name: "echo".into(),
                description: String::new(),
                input_schema: json!({}),
            }],
        };
        for value in [
            json!({"outcome":"await_host_tools", "calls":[["call", "echo", {}]]}),
            json!({"outcome":"completed", "text":"done", "metadata":["format", {}]}),
        ] {
            assert!(
                decode_step(value, &request).is_err(),
                "protocol structures must be JSON objects"
            );
        }
    }
    #[test]
    fn launch_requires_resolved_absolute_executable() {
        let command = ComponentCommand {
            executable: "from-path".into(),
            args: vec![],
            env: Default::default(),
        };
        assert_eq!(
            validate_command(&command)
                .expect_err("must not search PATH")
                .code,
            "invalid_config"
        );
    }
}
