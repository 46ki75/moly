//! Server-owned, single-invocation Provider processes. No model-specific encoding.
use crate::transport::{encode_frame, read_frame};
use moly_protocol::{Body, Message, ProtocolError, model::*};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(3);
const STEP_TIMEOUT: Duration = Duration::from_secs(65);

/// Mechanism for invoking the implementation selected by a resolved configuration.
pub struct ModelProvider;
impl ModelProvider {
    /// Validate options in the selected implementation, without model calls or secrets.
    pub async fn validate(&self, config: &ProviderConfig) -> Result<(), ProtocolError> {
        validate_command(&config.command)?;
        let mut process = Process::spawn(&config.command).await?;
        let result = process
            .request(
                2,
                "provider.validate",
                config.options.clone(),
                VALIDATION_TIMEOUT,
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

    /// One authorized step. Dropping this future kills its child, without retrying.
    #[tracing::instrument(skip_all, fields(
        session_id = %request.context.session_id,
        run_id = %request.context.run_id,
        model_call_id = %request.context.model_call_id,
    ))]
    pub async fn step(
        &self,
        config: &ProviderConfig,
        request: ModelRequest,
    ) -> Result<ModelStep, ProtocolError> {
        validate_command(&config.command)?;
        let params = serde_json::to_value(&request).map_err(|_| protocol_error())?;
        let mut process = Process::spawn(&config.command).await?;
        let result = process
            .request(2, "provider.step", params, STEP_TIMEOUT)
            .await
            .and_then(|value| decode_step(value, &request));
        process.stop().await;
        result
    }
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
            // Never forward its untrusted stderr into Server/user logs.
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
        tracing::debug!("Model Provider handshake completed");
        Ok(process)
    }

    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        deadline: Duration,
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
            let message = read_frame(&mut self.output)
                .await
                .map_err(|_| protocol_error())?
                .ok_or_else(unavailable)?;
            match message.body {
                Body::Response {
                    id: returned,
                    result,
                } if returned == id => Ok(result),
                Body::Error {
                    id: Some(returned),
                    error,
                } if returned == id => Err(redact(error)),
                _ => Err(protocol_error()),
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
        tracing::debug!("Model Provider process reaped");
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
