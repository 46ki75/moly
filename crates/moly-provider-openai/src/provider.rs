//! Nonstreaming Chat Completions conversion, HTTP, and replay validation.

use std::collections::HashSet;
use std::time::Duration;

use moly_protocol::ProtocolError;
use moly_protocol::model::{HostToolCall, ModelMessage, ModelRequest, ModelStep, ProviderMetadata};
use reqwest::header::{AUTHORIZATION, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MESSAGE_FORMAT: &str = "openai.chat_completion.message.v1";

#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Profile {
    #[default]
    Openai,
    OpencodeGo,
}

#[derive(Deserialize)]
struct Options {
    model_endpoint: String,
    model: String,
    #[serde(default)]
    profile: Profile,
}

impl Options {
    fn parse(value: Value) -> Result<(Self, reqwest::Url), ProtocolError> {
        // Serde unit enums also accept tagged objects; the profile option is a
        // JSON string, not an alternate {"opencode-go": null} representation.
        if !value.is_object()
            || value
                .get("profile")
                .is_some_and(|profile| !profile.is_string())
        {
            return Err(invalid_config());
        }
        let options: Self = serde_json::from_value(value).map_err(|_| invalid_config())?;
        let endpoint =
            reqwest::Url::parse(&options.model_endpoint).map_err(|_| invalid_config())?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || options.model.trim().is_empty()
            || options.model.len() > 256
        {
            return Err(invalid_config());
        }
        Ok((options, endpoint))
    }
}

fn invalid_config() -> ProtocolError {
    ProtocolError::new(
        "invalid_config",
        "Expected an HTTP(S) endpoint without credentials or fragments, a model name of at most 256 bytes, and profile openai or opencode-go",
    )
}

/// Validate only explicit implementation options, without discovery or HTTP.
pub(crate) fn validate(options: Value) -> Result<(), ProtocolError> {
    Options::parse(options).map(|_| ())
}

/// HTTP client with no redirects, ambient proxies, or credential discovery.
pub(crate) struct HttpProvider {
    client: reqwest::Client,
}

impl HttpProvider {
    /// Build an independent client with a deadline covering headers and body.
    pub(crate) fn new() -> Result<Self, ProtocolError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // A 307/308 must not forward prompts or credentials to another URL.
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| ProtocolError::new("provider_error", "Could not create HTTP provider"))?;
        Ok(Self { client })
    }

    /// Perform exactly one authorized inference at the complete supplied URL.
    #[tracing::instrument(
        name = "model_call",
        skip_all,
        fields(
            session_id = %input.context.session_id,
            run_id = %input.context.run_id,
            model_call_id = %input.context.model_call_id,
            call_kind = ?input.context.call_kind,
        )
    )]
    pub(crate) async fn step(&self, input: ModelRequest) -> Result<ModelStep, ProtocolError> {
        let (options, endpoint) = Options::parse(input.options)?;
        let mut names = HashSet::new();
        for tool in &input.tools {
            if !valid_tool_name(&tool.name)
                || !tool.input_schema.is_object()
                || !names.insert(tool.name.as_str())
            {
                return Err(ProtocolError::new(
                    "invalid_params",
                    "Expected unique function names and JSON Schema objects",
                ));
            }
        }
        let messages = input
            .messages
            .iter()
            .map(encode_message)
            .collect::<Result<Vec<_>, _>>()?;
        let mut body = json!({
            "model": options.model,
            "messages": messages,
            "stream": false,
            "n": 1,
        });
        if !input.tools.is_empty() {
            body["tools"] = Value::Array(
                input
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            },
                        })
                    })
                    .collect(),
            );
        }
        let mut request = self.client.post(endpoint).json(&body);
        if options.profile == Profile::OpencodeGo {
            if input.credential.is_none() {
                return Err(ProtocolError::new(
                    "invalid_secret",
                    "OpenCode Go requires a model credential",
                ));
            }
            // Go routes/caches by conversation, not run, HTTP client, or process.
            // https://opencode.ai/docs/go#where-can-i-use-it
            request = request
                .header("x-opencode-session", input.context.session_id.to_string())
                .header(USER_AGENT, concat!("moly/", env!("CARGO_PKG_VERSION")));
        }
        if let Some(secret) = input.credential {
            if secret.is_empty() || secret.bytes().any(|byte| !matches!(byte, 0x21..=0x7e)) {
                return Err(ProtocolError::new(
                    "invalid_secret",
                    "Model credential cannot be used for bearer authentication",
                ));
            }
            let mut authorization = HeaderValue::from_str(&format!("Bearer {secret}"))
                .map_err(|_| ProtocolError::new("invalid_secret", "Invalid model credential"))?;
            authorization.set_sensitive(true);
            request = request.header(AUTHORIZATION, authorization);
        }

        let mut response = request.send().await.map_err(redact_http_error)?;
        if !response.status().is_success() {
            // Error bodies may echo prompts or secrets, so never parse or log them.
            return Err(ProtocolError::new(
                "provider_error",
                "Model provider rejected the request",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(response_too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(redact_http_error)? {
            // Content-Length is optional and untrusted; bound every append too.
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(response_too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
        decode_response(value, &names)
    }
}

fn encode_message(message: &ModelMessage) -> Result<Value, ProtocolError> {
    match message {
        ModelMessage::User { text } => Ok(json!({"role": "user", "content": text})),
        ModelMessage::ToolResult { call_id, output } => Ok(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": serde_json::to_string(output).map_err(|_| invalid_response())?,
        })),
        ModelMessage::Assistant {
            text,
            tool_calls,
            metadata,
        } => {
            let raw = if let Some(metadata) = metadata {
                if metadata.format != MESSAGE_FORMAT {
                    return Err(unsupported_response());
                }
                metadata.value.clone()
            } else {
                let mut raw = json!({"role": "assistant", "content": text});
                if !tool_calls.is_empty() {
                    raw["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|call| {
                                Ok(json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": serde_json::to_string(&call.arguments)
                                            .map_err(|_| invalid_response())?,
                                    },
                                }))
                            })
                            .collect::<Result<Vec<_>, ProtocolError>>()?,
                    );
                }
                raw
            };
            // Historical tools need not still be advertised. Validate the replay
            // representation itself, then require exact normalized semantics.
            let (raw_text, raw_calls) = decode_message(&raw)?;
            if &raw_text != text
                || &raw_calls != tool_calls
                || (raw_text.is_none() && raw_calls.is_empty())
            {
                return Err(invalid_response());
            }
            Ok(raw)
        }
    }
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn redact_http_error(error: reqwest::Error) -> ProtocolError {
    if error.is_timeout() {
        ProtocolError::new("provider_timeout", "Model provider request timed out")
    } else {
        ProtocolError::new("provider_error", "Model provider request failed")
    }
}

fn invalid_response() -> ProtocolError {
    ProtocolError::new(
        "provider_response_invalid",
        "Invalid model provider response or replay metadata",
    )
}

fn unsupported_response() -> ProtocolError {
    ProtocolError::new(
        "provider_unsupported",
        "Unsupported model feature, finish reason, or replay format",
    )
}

fn response_too_large() -> ProtocolError {
    ProtocolError::new(
        "provider_response_too_large",
        "Model provider response exceeds 512 KiB",
    )
}

fn decode_response(value: Value, names: &HashSet<&str>) -> Result<ModelStep, ProtocolError> {
    let root = value.as_object().ok_or_else(invalid_response)?;
    if root
        .get("object")
        .is_some_and(|kind| kind != "chat.completion")
    {
        return Err(unsupported_response());
    }
    let choices = root
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| choices.len() == 1)
        .ok_or_else(invalid_response)?;
    let choice = choices[0].as_object().ok_or_else(invalid_response)?;
    if choice
        .get("index")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(invalid_response());
    }
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or_else(invalid_response)?;
    let message = choice.get("message").ok_or_else(invalid_response)?;
    let (text, calls) = decode_message(message)?;
    let metadata = Some(ProviderMetadata {
        format: MESSAGE_FORMAT.into(),
        value: message.clone(),
    });
    match finish {
        "stop" => {
            if !calls.is_empty() {
                return Err(invalid_response());
            }
            Ok(ModelStep::Completed {
                text: text.ok_or_else(invalid_response)?,
                metadata,
            })
        }
        "tool_calls" => {
            if calls.is_empty()
                || calls.len() > 32
                || calls.iter().any(|call| !names.contains(call.name.as_str()))
            {
                return Err(invalid_response());
            }
            Ok(ModelStep::AwaitHostTools {
                text,
                calls,
                metadata,
            })
        }
        // Truncation, filtering, and provider-native tools are never success.
        _ => Err(unsupported_response()),
    }
}

fn decode_message(message: &Value) -> Result<(Option<String>, Vec<HostToolCall>), ProtocolError> {
    let fields = message.as_object().ok_or_else(invalid_response)?;
    if fields.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(invalid_response());
    }
    // Nullable native fields are accepted, but their nonempty forms cannot be
    // silently discarded. Preserve all accepted fields in replay metadata.
    // Upstream schema: https://github.com/openai/openai-python/blob/main/src/openai/types/chat/chat_completion_message.py
    for (key, value) in fields {
        match key.as_str() {
            "role" | "content" | "tool_calls" => {}
            // Thinking models need this replayed on later tool steps. Keep it
            // opaque in metadata, never as assistant text or execution authority.
            // https://api-docs.deepseek.com/guides/thinking_mode/#tool-calls
            "reasoning_content" if value.is_null() || value.is_string() => {}
            "refusal" | "audio" | "function_call" if value.is_null() => {}
            "annotations" if value.is_null() || value.as_array().is_some_and(Vec::is_empty) => {}
            _ => return Err(unsupported_response()),
        }
    }
    let text = match fields.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        None | Some(Value::Null) => None,
        _ => return Err(unsupported_response()),
    };
    let calls = match fields.get("tool_calls") {
        Some(Value::Array(calls)) => calls.as_slice(),
        None | Some(Value::Null) => &[],
        _ => return Err(invalid_response()),
    };
    let mut ids = HashSet::new();
    let mut host_calls = Vec::with_capacity(calls.len());
    for call in calls {
        let call = call.as_object().ok_or_else(invalid_response)?;
        let kind = call
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(invalid_response)?;
        if kind != "function"
            || call
                .keys()
                .any(|key| !matches!(key.as_str(), "id" | "type" | "function"))
        {
            return Err(unsupported_response());
        }
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(invalid_response)?;
        if !ids.insert(id) {
            return Err(invalid_response());
        }
        let function = call
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(invalid_response)?;
        if function
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "arguments"))
        {
            return Err(unsupported_response());
        }
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_tool_name(name))
            .ok_or_else(invalid_response)?;
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(invalid_response)?;
        let arguments: Value = serde_json::from_str(arguments).map_err(|_| invalid_response())?;
        if !arguments.is_object() {
            return Err(invalid_response());
        }
        host_calls.push(HostToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        });
    }
    Ok((text, host_calls))
}

#[cfg(test)]
mod tests;
