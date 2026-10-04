//! Private, stateless Responses HTTP/SSE adapter; never log its inputs or bodies.
//!
//! Contract sources:
//! - https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference.md
//! - https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations.md
//! - https://developers.openai.com/api/reference/resources/responses/methods/create.md
//! - https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery.md

use crate::error::Error;
use moly_protocol::model::{HostToolCall, ModelMessage, ModelRequest, ModelStep, ProviderMetadata};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::time::Duration;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OUTPUT_FORMAT: &str = "openai.responses.output.v1";
const NAMESPACE: &str = "moly";
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_BYTES: usize = 768 * 1024;
const MAX_CALLS: usize = 32;
const DEADLINE: Duration = Duration::from_secs(60);

/// Encode only the supported SIWC fields, with all context supplied in `input`.
pub(crate) fn encode(request: &ModelRequest, model: &str) -> Result<Value, Error> {
    if model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err(Error::InvalidConfig);
    }
    advertised_names(request)?;
    let mut input = Vec::new();
    let mut seen_calls = HashSet::new();
    let mut pending = BTreeMap::new();
    for message in &request.messages {
        match message {
            ModelMessage::User { text } => {
                if !pending.is_empty() {
                    return Err(Error::Replay);
                }
                input.push(json!({"role": "user", "content": text}));
            }
            ModelMessage::Assistant {
                text,
                tool_calls,
                metadata,
            } => {
                if !pending.is_empty() {
                    return Err(Error::Replay);
                }
                // Historical calls describe already accepted context, not current
                // execution authority. Tools may have been removed since that turn.
                let names = tool_calls.iter().map(|call| call.name.as_str()).collect();
                let raw = match metadata {
                    Some(metadata) => {
                        if metadata.format != OUTPUT_FORMAT {
                            return Err(Error::Replay);
                        }
                        ensure_size(&metadata.value, MAX_OUTPUT_BYTES)?;
                        let normalized =
                            normalize_output(&metadata.value, &names).map_err(|_| Error::Replay)?;
                        if (normalized.text.is_none() && normalized.calls.is_empty())
                            || normalized.text.as_ref() != text.as_ref()
                            || normalized.calls != *tool_calls
                        {
                            return Err(Error::Replay);
                        }
                        metadata.value.as_array().ok_or(Error::Replay)?.clone()
                    }
                    None => {
                        validate_calls(tool_calls, &names).map_err(|_| Error::Replay)?;
                        if text.is_none() && tool_calls.is_empty() {
                            return Err(Error::Replay);
                        }
                        let mut items = Vec::new();
                        if let Some(text) = text {
                            items.push(json!({"role": "assistant", "content": text}));
                        }
                        for call in tool_calls {
                            // FunctionCall uses flat name/arguments and a separate namespace,
                            // not Chat Completions' nested `function` or a qualified name.
                            items.push(json!({
                                "type": "function_call", "namespace": NAMESPACE,
                                "call_id": call.id, "name": call.name,
                                "arguments": serde_json::to_string(&call.arguments)
                                    .map_err(|_| Error::Internal)?
                            }));
                        }
                        ensure_size(&items, MAX_OUTPUT_BYTES)?;
                        items
                    }
                };
                for call in tool_calls {
                    if !seen_calls.insert(call.id.clone()) {
                        return Err(Error::Replay);
                    }
                    pending.insert(call.id.clone(), call.name.clone());
                }
                input.extend(raw);
            }
            ModelMessage::ToolResult { call_id, output } => {
                let name = pending.remove(call_id).ok_or(Error::Replay)?;
                input.push(json!({
                    "type": "function_call_output", "call_id": call_id,
                    "name": name, "namespace": NAMESPACE,
                    "output": serde_json::to_string(output).map_err(|_| Error::Internal)?
                }));
            }
        }
    }
    if !pending.is_empty() {
        return Err(Error::Replay);
    }
    // Preview limitations require namespace grouping, store:false, stream:true,
    // and full context instead of previous_response_id. Never merge opaque options.
    let mut body = json!({
        "model": model, "input": input, "stream": true, "store": false,
        "include": ["reasoning.encrypted_content"]
    });
    if !request.tools.is_empty() {
        let tools: Vec<_> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function", "name": tool.name, "description": tool.description,
                    "parameters": tool.input_schema, "strict": false
                })
            })
            .collect();
        body["tools"] = json!([{
            "type": "namespace", "name": NAMESPACE,
            "description": "Server-advertised hosted capabilities", "tools": tools
        }]);
    }
    ensure_size(&body, MAX_BODY_BYTES)?;
    Ok(body)
}

/// Perform one inference without retries; dropping this future drops HTTP work.
/// The owner must disable client retries, redirects, and ambient proxies.
pub(crate) async fn infer(
    client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    request: &ModelRequest,
    model: &str,
) -> Result<ModelStep, Error> {
    validate_endpoint(endpoint)?;
    if access_token.trim().is_empty() || access_token.chars().any(char::is_control) {
        return Err(Error::AuthRequired);
    }
    let body = encode(request, model)?;
    let names = advertised_names(request)?;
    // Both reqwest's request timeout and the enclosing deadline cover the body,
    // not just headers. No stream failure initiates a second POST.
    tokio::time::timeout(DEADLINE, async {
        let mut response = client
            .post(endpoint)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .timeout(DEADLINE)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    Error::Timeout
                } else {
                    Error::Network
                }
            })?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BODY_BYTES as u64)
        {
            return Err(Error::TooLarge);
        }
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(body_error)? {
                if chunk.len() > MAX_BODY_BYTES - bytes.len() {
                    return Err(Error::TooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            let parsed = serde_json::from_slice::<Value>(&bytes).ok();
            let code = parsed
                .as_ref()
                .and_then(|value| value.get("error"))
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str);
            return Err(classify_error(code, Some(status.as_u16())));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next());
        if !content_type.is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(Error::InvalidResponse);
        }
        let mut parser = SseParser::default();
        let mut state = ResponseState::default();
        while let Some(chunk) = response.chunk().await.map_err(body_error)? {
            parser.feed(&chunk, |data, event| state.accept(data, event, &names))?;
            if let Some(step) = state.completed.take() {
                return Ok(step);
            }
        }
        // EOF never dispatches a partial SSE event, even one containing completed.
        Err(Error::Interrupted)
    })
    .await
    .map_err(|_| Error::Timeout)?
}

fn validate_endpoint(endpoint: &str) -> Result<(), Error> {
    if endpoint == ENDPOINT {
        return Ok(());
    }
    // Endpoint injection is deliberately confined to loopback unit tests; no
    // production fallback to backend-api, custom hosts, or API-key billing.
    #[cfg(test)]
    if let Ok(url) = reqwest::Url::parse(endpoint)
        && url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/v1/responses"
        && url.query().is_none()
        && url.fragment().is_none()
    {
        return Ok(());
    }
    Err(Error::InvalidConfig)
}

fn body_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Interrupted
    }
}

fn classify_error(code: Option<&str>, status: Option<u16>) -> Error {
    // Exact SIWC codes from errors-and-recovery.md; never expose diagnostic text
    // or assume an admission body has the structured API error shape.
    match code {
        Some("subscription_sharing_usage_limit_exceeded") => Error::UsageLimit,
        Some("subscription_sharing_usage_unavailable") => Error::UsageUnavailable,
        Some("subscription_sharing_unsupported_capability") => Error::Unsupported,
        Some("subscription_sharing_invalid_user" | "invalid_api_key" | "token_expired") => {
            Error::AuthRequired
        }
        Some(
            "subscription_sharing_user_not_eligible"
            | "subscription_sharing_route_not_supported"
            | "chatpass_v2_scope_not_authorized"
            | "chatpass_v2_invalid_authorization_context",
        ) => Error::PermissionDenied,
        _ => match status {
            Some(401) => Error::AuthRequired,
            Some(403) => Error::PermissionDenied,
            Some(408 | 504) => Error::Timeout,
            Some(413) => Error::TooLarge,
            Some(429) => Error::UsageLimit,
            Some(400 | 422) => Error::InvalidParams,
            Some(300..=399) => Error::InvalidResponse,
            _ => Error::ProviderFailed,
        },
    }
}

fn advertised_names(request: &ModelRequest) -> Result<HashSet<&str>, Error> {
    let mut names = HashSet::new();
    for tool in &request.tools {
        if !valid_name(&tool.name)
            || !tool.input_schema.is_object()
            || !names.insert(tool.name.as_str())
        {
            return Err(Error::InvalidParams);
        }
    }
    Ok(names)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && !id.chars().any(char::is_control)
}

fn validate_calls(calls: &[HostToolCall], names: &HashSet<&str>) -> Result<(), Error> {
    if calls.len() > MAX_CALLS {
        return Err(Error::InvalidResponse);
    }
    let mut ids = HashSet::new();
    for call in calls {
        if !valid_id(&call.id)
            || !valid_name(&call.name)
            || !call.arguments.is_object()
            || !ids.insert(&call.id)
        {
            return Err(Error::InvalidResponse);
        }
        if !names.contains(call.name.as_str()) {
            return Err(Error::Unsupported);
        }
    }
    Ok(())
}

struct NormalizedOutput {
    text: Option<String>,
    calls: Vec<HostToolCall>,
}

fn normalize_output(output: &Value, names: &HashSet<&str>) -> Result<NormalizedOutput, Error> {
    ensure_size(output, MAX_OUTPUT_BYTES)?;
    let items = output.as_array().ok_or(Error::InvalidResponse)?;
    let mut text = None;
    let mut calls = Vec::new();
    let mut item_ids = HashSet::new();
    for item in items {
        if let Some(id) = item.get("id") {
            let id = id
                .as_str()
                .filter(|id| valid_id(id))
                .ok_or(Error::InvalidResponse)?;
            if !item_ids.insert(id) {
                return Err(Error::InvalidResponse);
            }
        }
        validate_item(item, names)?;
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().ok_or(Error::InvalidResponse)? {
                    let part_text = part["text"].as_str().ok_or(Error::InvalidResponse)?;
                    text.get_or_insert_with(String::new).push_str(part_text);
                }
            }
            Some("function_call") => calls.push(HostToolCall {
                id: string(item, "call_id")?.to_owned(),
                name: string(item, "name")?.to_owned(),
                arguments: serde_json::from_str(string(item, "arguments")?)
                    .map_err(|_| Error::InvalidResponse)?,
            }),
            Some("reasoning") => {}
            _ => return Err(Error::Unsupported),
        }
    }
    validate_calls(&calls, names)?;
    Ok(NormalizedOutput { text, calls })
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(Error::InvalidResponse)
}

fn fields(value: &Value, allowed: &[&str]) -> Result<(), Error> {
    let object = value.as_object().ok_or(Error::InvalidResponse)?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::Unsupported);
    }
    Ok(())
}

fn completed_status(item: &Value, required: bool) -> Result<(), Error> {
    match item.get("status").and_then(Value::as_str) {
        Some("completed") => Ok(()),
        Some("in_progress" | "incomplete") => Err(Error::Incomplete),
        None if !required && item.get("status").is_none() => Ok(()),
        _ => Err(Error::InvalidResponse),
    }
}

fn validate_item(item: &Value, names: &HashSet<&str>) -> Result<(), Error> {
    match string(item, "type")? {
        "message" => {
            fields(item, &["type", "id", "role", "status", "content", "phase"])?;
            if !valid_id(string(item, "id")?) || string(item, "role")? != "assistant" {
                return Err(Error::InvalidResponse);
            }
            completed_status(item, true)?;
            if let Some(phase) = item.get("phase")
                && !phase.is_null()
                && !matches!(phase.as_str(), Some("commentary" | "final_answer"))
            {
                return Err(Error::InvalidResponse);
            }
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .ok_or(Error::InvalidResponse)?
            {
                match string(part, "type")? {
                    "output_text" => {
                        fields(part, &["type", "text", "annotations", "logprobs"])?;
                        string(part, "text")?;
                        if !part.get("annotations").is_some_and(Value::is_array)
                            || part.get("logprobs").is_some_and(|value| !value.is_array())
                        {
                            return Err(Error::InvalidResponse);
                        }
                    }
                    _ => return Err(Error::Unsupported),
                }
            }
        }
        "function_call" => {
            fields(
                item,
                &[
                    "type",
                    "id",
                    "call_id",
                    "name",
                    "arguments",
                    "namespace",
                    "status",
                    "async",
                    "caller",
                ],
            )?;
            completed_status(item, false)?;
            if string(item, "namespace")? != NAMESPACE || !names.contains(string(item, "name")?) {
                return Err(Error::Unsupported);
            }
            if !valid_id(string(item, "call_id")?) {
                return Err(Error::InvalidResponse);
            }
            let arguments: Value = serde_json::from_str(string(item, "arguments")?)
                .map_err(|_| Error::InvalidResponse)?;
            if !arguments.is_object() {
                return Err(Error::InvalidResponse);
            }
            if let Some(asynchronous) = item.get("async") {
                match asynchronous.as_bool() {
                    Some(false) => {}
                    Some(true) => return Err(Error::Unsupported),
                    None => return Err(Error::InvalidResponse),
                }
            }
            if let Some(caller) = item.get("caller")
                && !caller.is_null()
            {
                fields(caller, &["type"])?;
                if string(caller, "type")? != "direct" {
                    return Err(Error::Unsupported);
                }
            }
        }
        "reasoning" => {
            fields(
                item,
                &[
                    "type",
                    "id",
                    "summary",
                    "content",
                    "encrypted_content",
                    "status",
                ],
            )?;
            if !valid_id(string(item, "id")?) {
                return Err(Error::InvalidResponse);
            }
            completed_status(item, false)?;
            for part in item
                .get("summary")
                .and_then(Value::as_array)
                .ok_or(Error::InvalidResponse)?
            {
                fields(part, &["type", "text"])?;
                if string(part, "type")? != "summary_text" {
                    return Err(Error::InvalidResponse);
                }
                string(part, "text")?;
            }
            if let Some(content) = item.get("content") {
                for part in content.as_array().ok_or(Error::InvalidResponse)? {
                    fields(part, &["type", "text"])?;
                    if string(part, "type")? != "reasoning_text" {
                        return Err(Error::InvalidResponse);
                    }
                    string(part, "text")?;
                }
            }
            if let Some(encrypted) = item.get("encrypted_content")
                && !encrypted.is_null()
                && !encrypted.is_string()
            {
                return Err(Error::InvalidResponse);
            }
        }
        _ => return Err(Error::Unsupported),
    }
    Ok(())
}

// Count JSON bytes without allocating another copy of sensitive output/replay.
fn ensure_size<T: Serialize + ?Sized>(value: &T, limit: usize) -> Result<(), Error> {
    struct Budget {
        left: usize,
        exceeded: bool,
    }
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.left {
                self.exceeded = true;
                return Err(io::Error::other("JSON size limit"));
            }
            self.left -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut budget = Budget {
        left: limit,
        exceeded: false,
    };
    serde_json::to_writer(&mut budget, value).map_err(|_| {
        if budget.exceeded {
            Error::TooLarge
        } else {
            Error::Internal
        }
    })
}

#[derive(Default)]
struct ResponseState {
    response_id: Option<String>,
    sequence: Option<u64>,
    done: BTreeMap<usize, Value>,
    done_bytes: usize,
    completed: Option<ModelStep>,
}

impl ResponseState {
    fn accept(
        &mut self,
        data: &str,
        event: Option<&str>,
        names: &HashSet<&str>,
    ) -> Result<bool, Error> {
        if self.completed.is_some() {
            return Err(Error::InvalidResponse);
        }
        if data == "[DONE]" {
            return Err(Error::Interrupted);
        }
        let value: Value = serde_json::from_str(data).map_err(|_| Error::InvalidResponse)?;
        let kind = string(&value, "type")?;
        if event.is_some_and(|event| !event.is_empty() && event != "message" && event != kind) {
            return Err(Error::InvalidResponse);
        }
        if let Some(sequence) = value.get("sequence_number") {
            let sequence = sequence.as_u64().ok_or(Error::InvalidResponse)?;
            if self.sequence.is_some_and(|previous| sequence <= previous) {
                return Err(Error::InvalidResponse);
            }
            self.sequence = Some(sequence);
        }
        if let Some(response) = value.get("response") {
            let id = string(response, "id")?;
            if !valid_id(id)
                || self
                    .response_id
                    .as_ref()
                    .is_some_and(|previous| previous != id)
            {
                return Err(Error::InvalidResponse);
            }
            self.response_id = Some(id.to_owned());
        }
        match kind {
            "response.failed" => {
                let response = value.get("response").ok_or(Error::InvalidResponse)?;
                let code = response
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_str);
                Err(classify_error(code, None))
            }
            "error" => {
                let error = match value.get("error") {
                    Some(error) => error,
                    None => &value,
                };
                Err(classify_error(
                    error.get("code").and_then(Value::as_str),
                    None,
                ))
            }
            "response.incomplete" => Err(Error::Incomplete),
            "response.completed" => {
                let response = value.get("response").ok_or(Error::InvalidResponse)?;
                if string(response, "status")? != "completed"
                    || response.get("error").is_some_and(|value| !value.is_null())
                    || response
                        .get("incomplete_details")
                        .is_some_and(|value| !value.is_null())
                {
                    return Err(Error::InvalidResponse);
                }
                let mut output = response
                    .get("output")
                    .ok_or(Error::InvalidResponse)?
                    .clone();
                let items = output.as_array_mut().ok_or(Error::InvalidResponse)?;
                for (index, done) in &self.done {
                    let item = items.get_mut(*index).ok_or(Error::InvalidResponse)?;
                    // Create docs explicitly require the completed encrypted reasoning
                    // from output_item.done (added may contain only partial ciphertext).
                    // Permit completed to omit it, but reject conflicting final items.
                    if done["type"] == "reasoning" {
                        let mut left = item.clone();
                        let mut right = done.clone();
                        left.as_object_mut()
                            .ok_or(Error::InvalidResponse)?
                            .remove("encrypted_content");
                        right
                            .as_object_mut()
                            .ok_or(Error::InvalidResponse)?
                            .remove("encrypted_content");
                        if left != right {
                            return Err(Error::InvalidResponse);
                        }
                        if let Some(encrypted) = done
                            .get("encrypted_content")
                            .filter(|value| !value.is_null())
                        {
                            if item
                                .get("encrypted_content")
                                .is_some_and(|value| !value.is_null() && value != encrypted)
                            {
                                return Err(Error::InvalidResponse);
                            }
                            item["encrypted_content"] = encrypted.clone();
                        }
                    } else if item != done {
                        return Err(Error::InvalidResponse);
                    }
                }
                let normalized = normalize_output(&output, names)?;
                let metadata = Some(ProviderMetadata {
                    format: OUTPUT_FORMAT.into(),
                    value: output,
                });
                let step = if normalized.calls.is_empty() {
                    ModelStep::Completed {
                        text: normalized.text.ok_or(Error::InvalidResponse)?,
                        metadata,
                    }
                } else {
                    ModelStep::AwaitHostTools {
                        text: normalized.text,
                        calls: normalized.calls,
                        metadata,
                    }
                };
                ensure_size(&step, MAX_OUTPUT_BYTES)?;
                self.completed = Some(step);
                Ok(())
            }
            "response.output_item.added" => {
                match string(value.get("item").ok_or(Error::InvalidResponse)?, "type")? {
                    "message" | "function_call" | "reasoning" => Ok(()),
                    _ => Err(Error::Unsupported),
                }
            }
            "response.output_item.done" => {
                let index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or(Error::InvalidResponse)?;
                let item = value.get("item").ok_or(Error::InvalidResponse)?;
                validate_item(item, names)?;
                let size = serde_json::to_vec(item).map_err(|_| Error::Internal)?.len();
                if size > MAX_OUTPUT_BYTES - self.done_bytes {
                    return Err(Error::TooLarge);
                }
                self.done_bytes += size;
                if self.done.insert(index, item.clone()).is_some() {
                    return Err(Error::InvalidResponse);
                }
                Ok(())
            }
            "response.created" | "response.in_progress" => {
                let response = value.get("response").ok_or(Error::InvalidResponse)?;
                if string(response, "status")? != "in_progress" {
                    return Err(Error::InvalidResponse);
                }
                Ok(())
            }
            // Deltas are not execution authority or final output. Only the
            // terminal response's validated array becomes a ModelStep.
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.output_text.annotation.added"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => Ok(()),
            _ => Err(Error::Unsupported),
        }?;
        Ok(self.completed.is_some())
    }
}

// WHATWG SSE framing: UTF-8, optional leading BOM, LF/CR/CRLF, data lines
// joined with LF, dispatch only at a blank line, comments ignored.
// https://html.spec.whatwg.org/multipage/server-sent-events.html#event-stream-interpretation
// Unlike EventSource, this parser never reconnects or acts on retry/id fields.
#[derive(Default)]
struct SseParser {
    total: usize,
    event_bytes: usize,
    line: Vec<u8>,
    data: String,
    event: Option<String>,
    skip_lf: bool,
    cr_event_bytes: usize,
    started: bool,
}

impl SseParser {
    fn feed(
        &mut self,
        bytes: &[u8],
        mut accept: impl FnMut(&str, Option<&str>) -> Result<bool, Error>,
    ) -> Result<(), Error> {
        for &byte in bytes {
            if self.total == MAX_BODY_BYTES {
                return Err(Error::TooLarge);
            }
            self.total += 1;
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    // Count both bytes of CRLF even when the CR dispatched and
                    // reset the event, or when the pair spans HTTP chunks.
                    if self.cr_event_bytes == MAX_EVENT_BYTES {
                        return Err(Error::TooLarge);
                    }
                    if self.event_bytes != 0 {
                        self.event_bytes += 1;
                    }
                    continue;
                }
            }
            self.event_bytes += 1;
            if self.event_bytes > MAX_EVENT_BYTES {
                return Err(Error::TooLarge);
            }
            if matches!(byte, b'\r' | b'\n') {
                self.skip_lf = byte == b'\r';
                self.cr_event_bytes = self.event_bytes;
                let line = std::mem::take(&mut self.line);
                let mut line = std::str::from_utf8(&line).map_err(|_| Error::InvalidResponse)?;
                if !self.started {
                    self.started = true;
                    line = line.strip_prefix('\u{feff}').unwrap_or(line);
                }
                if line.is_empty() {
                    if !self.data.is_empty() {
                        self.data.pop();
                        // Stop at the validated terminal event, not the end of an
                        // arbitrary HTTP chunk. Trailers are outside this response.
                        if accept(&self.data, self.event.as_deref())? {
                            return Ok(());
                        }
                    }
                    self.data.clear();
                    self.event = None;
                    self.event_bytes = 0;
                } else if !line.starts_with(':') {
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "data" => {
                            self.data.push_str(value);
                            self.data.push('\n');
                        }
                        "event" => self.event = Some(value.to_owned()),
                        _ => {}
                    }
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
