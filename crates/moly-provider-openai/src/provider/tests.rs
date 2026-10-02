use super::*;
use moly_protocol::model::{CallKind, InferenceContext};
use moly_protocol::{ModelCallId, RunId, SessionId, ToolDefinition};
use std::error::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

type TestError = Box<dyn Error + Send + Sync>;
type TestResult = Result<(), TestError>;

struct CapturedRequest {
    headers: String,
    body: Value,
}

async fn capture_request(stream: &mut TcpStream) -> Result<CapturedRequest, TestError> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 || bytes.len() + count > MAX_RESPONSE_BYTES {
            return Err("invalid mock request headers".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec())?;
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .ok_or("missing mock request content length")?
        .1
        .trim()
        .parse()?;
    if header_end + length > MAX_RESPONSE_BYTES {
        return Err("mock request too large".into());
    }
    while bytes.len() < header_end + length {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Err("incomplete mock request".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + length])?;
    Ok(CapturedRequest { headers, body })
}

async fn mock_http(
    status: &str,
    body: Vec<u8>,
    chunked: bool,
    extra_headers: &str,
) -> Result<(String, JoinHandle<Result<CapturedRequest, TestError>>), TestError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/chat/completions", listener.local_addr()?);
    let headers = if chunked {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n{extra_headers}\r\n"
        )
    } else {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        )
    };
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let (mut stream, _) = listener.accept().await?;
            let request = capture_request(&mut stream).await?;
            // Early rejection may close the connection during writes.
            if stream.write_all(headers.as_bytes()).await.is_ok() {
                if chunked {
                    for chunk in body.chunks(4096) {
                        let frame = format!("{:x}\r\n", chunk.len());
                        if stream.write_all(frame.as_bytes()).await.is_err()
                            || stream.write_all(chunk).await.is_err()
                            || stream.write_all(b"\r\n").await.is_err()
                        {
                            break;
                        }
                    }
                    let _ = stream.write_all(b"0\r\n\r\n").await;
                } else {
                    let _ = stream.write_all(&body).await;
                }
            }
            Ok::<_, TestError>(request)
        })
        .await?
    });
    Ok((endpoint, task))
}

fn request(endpoint: String) -> ModelRequest {
    ModelRequest {
        options: json!({"model_endpoint": endpoint, "model": "mock-model"}),
        credential: None,
        context: InferenceContext {
            session_id: SessionId::new(),
            run_id: RunId::new(),
            model_call_id: ModelCallId::new(),
            call_kind: CallKind::Primary,
        },
        messages: vec![],
        tools: vec![],
    }
}

fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".into(),
        description: "Read a workspace file".into(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }
}

fn completion(message: Value, finish: &str) -> Value {
    json!({"object": "chat.completion", "choices": [{"index": 0, "message": message, "finish_reason": finish}]})
}

fn tool_message() -> Value {
    json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{"id": "call-1", "type": "function", "function": {
            "name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"
        }}],
    })
}

fn error_code<T>(result: Result<T, ProtocolError>) -> Result<String, TestError> {
    match result {
        Ok(_) => Err("expected provider failure".into()),
        Err(error) => Ok(error.code),
    }
}

#[test]
fn oversized_tool_batch_is_not_emitted_outside_the_protocol_contract() -> TestResult {
    let names = HashSet::from(["read_file"]);
    for count in [32, 33] {
        let mut message = tool_message();
        let call = message["tool_calls"][0].clone();
        message["tool_calls"] = Value::Array(
            (0..count)
                .map(|index| {
                    let mut call = call.clone();
                    call["id"] = json!(format!("call-{index}"));
                    call
                })
                .collect(),
        );
        let result = decode_response(completion(message, "tool_calls"), &names);
        if count == 32 {
            assert!(result.is_ok());
        } else {
            assert_eq!(error_code(result)?, "provider_response_invalid");
        }
    }
    Ok(())
}

#[tokio::test]
async fn text_completion_preserves_message_and_uses_exact_endpoint() -> TestResult {
    let message = json!({"role": "assistant", "content": "hello", "refusal": null});
    let (endpoint, task) = mock_http(
        "200 OK",
        serde_json::to_vec(&completion(message.clone(), "stop"))?,
        false,
        "",
    )
    .await?;
    let mut input = request(format!("{endpoint}?exact=query"));
    input.messages = vec![ModelMessage::User {
        text: "private prompt".into(),
    }];
    match HttpProvider::new()?.step(input).await? {
        ModelStep::Completed { text, metadata } => {
            assert_eq!(text, "hello");
            let metadata = metadata.ok_or("missing replay metadata")?;
            assert_eq!(metadata.format, MESSAGE_FORMAT);
            assert_eq!(metadata.value, message);
        }
        _ => return Err("expected text completion".into()),
    }
    let request = task.await??;
    assert!(
        request
            .headers
            .starts_with("POST /v1/chat/completions?exact=query HTTP/1.1\r\n")
    );
    assert!(
        !request
            .headers
            .to_ascii_lowercase()
            .contains("authorization:")
    );
    assert!(
        !request
            .headers
            .to_ascii_lowercase()
            .contains("x-opencode-session:")
    );
    assert_eq!(request.body["model"], "mock-model");
    assert_eq!(
        request.body["messages"],
        json!([{"role": "user", "content": "private prompt"}])
    );
    assert_eq!(request.body["stream"], false);
    assert_eq!(request.body["n"], 1);
    assert!(request.body.get("tools").is_none());
    Ok(())
}

#[tokio::test]
async fn opencode_go_headers_use_conversation_identity_not_run_or_process() -> TestResult {
    let session = SessionId::new();
    for current_session in [session, session, SessionId::new()] {
        let (endpoint, task) = mock_http(
            "200 OK",
            serde_json::to_vec(&completion(
                json!({"role": "assistant", "content": "ok"}),
                "stop",
            ))?,
            false,
            "",
        )
        .await?;
        let mut input = request(endpoint);
        input.options["profile"] = json!("opencode-go");
        input.context.session_id = current_session;
        input.credential = Some("go-test-key".into());
        // Each invocation has a fresh HTTP client, RunId, and ModelCallId.
        HttpProvider::new()?.step(input).await?;
        let captured = task.await??;
        let headers = captured.headers.to_ascii_lowercase();
        assert!(headers.contains(&format!("x-opencode-session: {current_session}\r\n")));
        assert!(headers.contains(&format!(
            "user-agent: moly/{}\r\n",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(headers.contains("authorization: bearer go-test-key\r\n"));
    }
    Ok(())
}

#[test]
fn opencode_go_profile_is_explicit_and_unknown_profiles_are_rejected() -> TestResult {
    for profile in [json!("openai"), json!("opencode-go")] {
        validate(
            json!({"profile": profile, "model_endpoint": "https://example.invalid/chat/completions", "model": "test"}),
        )?;
    }
    for profile in [json!("unknown-private-profile"), json!(null), json!(true)] {
        let error = validate(json!({"profile": profile, "model_endpoint": "https://example.invalid/", "model": "test"}))
            .err().ok_or("expected invalid profile")?;
        assert_eq!(error.code, "invalid_config");
        assert!(!error.message.contains("private"));
    }
    Ok(())
}

#[tokio::test]
async fn non_string_profiles_are_rejected_by_validation_and_inference() -> TestResult {
    let provider = HttpProvider::new()?;
    for profile in [
        json!(null),
        json!(true),
        json!(7),
        json!([]),
        json!({"openai": null}),
        json!({"opencode-go": null}),
    ] {
        let mut input = request("http://127.0.0.1:1/".into());
        input.options["profile"] = profile;
        assert_eq!(
            error_code(validate(input.options.clone()))?,
            "invalid_config"
        );
        assert_eq!(error_code(provider.step(input).await)?, "invalid_config");
    }
    Ok(())
}

#[tokio::test]
async fn opencode_go_requires_a_credential_before_http() -> TestResult {
    let mut input = request("http://127.0.0.1:1/".into());
    input.options["profile"] = json!("opencode-go");
    assert_eq!(
        error_code(HttpProvider::new()?.step(input).await)?,
        "invalid_secret"
    );
    Ok(())
}

#[tokio::test]
async fn opencode_go_preserves_reasoning_metadata_through_tool_replay() -> TestResult {
    let mut raw = tool_message();
    raw["reasoning_content"] = json!("opaque-go-reasoning");
    let (endpoint, first) = mock_http(
        "200 OK",
        serde_json::to_vec(&completion(raw.clone(), "tool_calls"))?,
        false,
        "",
    )
    .await?;
    let mut input = request(endpoint);
    input.options["profile"] = json!("opencode-go");
    input.credential = Some("go-test-key".into());
    input.tools = vec![tool()];
    let ModelStep::AwaitHostTools {
        text,
        calls,
        metadata,
    } = HttpProvider::new()?.step(input.clone()).await?
    else {
        return Err("expected hosted tools".into());
    };
    first.await??;
    assert!(text.is_none());
    assert_eq!(metadata.as_ref().ok_or("missing metadata")?.value, raw);
    let (endpoint, second) = mock_http(
        "200 OK",
        serde_json::to_vec(&completion(
            json!({"role": "assistant", "content": "done", "reasoning_content": null}),
            "stop",
        ))?,
        false,
        "",
    )
    .await?;
    input.options["model_endpoint"] = json!(endpoint);
    input.messages = vec![
        ModelMessage::Assistant {
            text,
            tool_calls: calls,
            metadata,
        },
        ModelMessage::ToolResult {
            call_id: "call-1".into(),
            output: json!({"content": "notes"}),
        },
    ];
    assert!(
        matches!(HttpProvider::new()?.step(input).await?, ModelStep::Completed { text, .. } if text == "done")
    );
    assert_eq!(second.await??.body["messages"][0], raw);
    Ok(())
}

#[tokio::test]
async fn opencode_go_rejects_malformed_reasoning_in_responses_and_replay() -> TestResult {
    for value in [json!({"private": true}), json!(["private"]), json!(1)] {
        let raw = json!({"role": "assistant", "content": "text", "reasoning_content": value});
        let (endpoint, task) = mock_http(
            "200 OK",
            serde_json::to_vec(&completion(raw.clone(), "stop"))?,
            false,
            "",
        )
        .await?;
        let mut input = request(endpoint);
        input.options["profile"] = json!("opencode-go");
        input.credential = Some("go-test-key".into());
        assert_eq!(
            error_code(HttpProvider::new()?.step(input.clone()).await)?,
            "provider_unsupported"
        );
        task.await??;
        input.options["model_endpoint"] = json!("http://127.0.0.1:1/");
        input.messages = vec![ModelMessage::Assistant {
            text: Some("text".into()),
            tool_calls: vec![],
            metadata: Some(ProviderMetadata {
                format: MESSAGE_FORMAT.into(),
                value: raw,
            }),
        }];
        assert_eq!(
            error_code(HttpProvider::new()?.step(input).await)?,
            "provider_unsupported"
        );
    }
    Ok(())
}

#[tokio::test]
async fn host_tools_and_bearer_auth_are_explicit() -> TestResult {
    let mut message = tool_message();
    message["content"] = json!("Let me check");
    let mut second = message["tool_calls"][0].clone();
    second["id"] = json!("call-2");
    message["tool_calls"]
        .as_array_mut()
        .ok_or("missing calls")?
        .push(second);
    let (endpoint, task) = mock_http(
        "200 OK",
        serde_json::to_vec(&completion(message.clone(), "tool_calls"))?,
        false,
        "",
    )
    .await?;
    let mut input = request(endpoint);
    input.credential = Some("test-credential".into());
    let definition = tool();
    input.tools = vec![definition.clone()];
    match HttpProvider::new()?.step(input).await? {
        ModelStep::AwaitHostTools {
            text,
            metadata,
            calls,
        } => {
            assert_eq!(text.as_deref(), Some("Let me check"));
            let metadata = metadata.ok_or("missing replay metadata")?;
            assert_eq!(metadata.format, MESSAGE_FORMAT);
            assert_eq!(metadata.value, message);
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].id, "call-1");
            assert_eq!(calls[1].id, "call-2");
            assert_eq!(calls[0].name, "read_file");
            assert_eq!(calls[0].arguments, json!({"path": "notes.txt"}));
        }
        _ => return Err("expected host tool calls".into()),
    }
    let request = task.await??;
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-credential\r\n")
    );
    assert_eq!(
        request.body["tools"],
        json!([{
            "type": "function", "function": {
                "name": definition.name, "description": definition.description,
                "parameters": definition.input_schema,
            }
        }])
    );
    Ok(())
}

#[tokio::test]
async fn errors_do_not_echo_provider_body_or_endpoint() -> TestResult {
    let sentinel = "private-provider-diagnostic";
    let (endpoint, task) =
        mock_http("401 Unauthorized", sentinel.as_bytes().to_vec(), false, "").await?;
    let error = match HttpProvider::new()?
        .step(request(format!("{endpoint}?private-query")))
        .await
    {
        Err(error) => error,
        Ok(_) => return Err("expected HTTP failure".into()),
    };
    assert_eq!(error.code, "provider_error");
    let rendered = serde_json::to_string(&error)?;
    assert!(!rendered.contains(sentinel));
    assert!(!rendered.contains("private-query"));
    task.await??;
    Ok(())
}

#[tokio::test]
async fn redirects_are_not_followed() -> TestResult {
    for profile in ["openai", "opencode-go"] {
        let target = TcpListener::bind("127.0.0.1:0").await?;
        let location = format!("Location: http://{}/other\r\n", target.local_addr()?);
        let (endpoint, task) =
            mock_http("307 Temporary Redirect", vec![], false, &location).await?;
        let mut input = request(endpoint);
        input.options["profile"] = json!(profile);
        input.credential = Some("private-credential".into());
        assert_eq!(
            error_code(HttpProvider::new()?.step(input).await)?,
            "provider_error"
        );
        task.await??;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), target.accept())
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn response_limit_applies_to_content_length_and_chunked_bodies() -> TestResult {
    for chunked in [false, true] {
        let (endpoint, task) =
            mock_http("200 OK", vec![b' '; MAX_RESPONSE_BYTES + 1], chunked, "").await?;
        assert_eq!(
            error_code(HttpProvider::new()?.step(request(endpoint)).await)?,
            "provider_response_too_large"
        );
        task.await??;
    }
    Ok(())
}

#[tokio::test]
async fn exact_response_limit_is_accepted() -> TestResult {
    let mut value = completion(json!({"role": "assistant", "content": "ok"}), "stop");
    value["padding"] = json!("");
    let overhead = serde_json::to_vec(&value)?.len();
    value["padding"] = json!(" ".repeat(MAX_RESPONSE_BYTES - overhead));
    let body = serde_json::to_vec(&value)?;
    assert_eq!(body.len(), MAX_RESPONSE_BYTES);
    let (endpoint, task) = mock_http("200 OK", body, true, "").await?;
    let step = HttpProvider::new()?.step(request(endpoint)).await?;
    assert!(matches!(step, ModelStep::Completed { text, .. } if text == "ok"));
    task.await??;
    Ok(())
}

#[tokio::test]
async fn malformed_json_is_redacted() -> TestResult {
    let (endpoint, task) = mock_http("200 OK", b"private-invalid-json".to_vec(), false, "").await?;
    let error = HttpProvider::new()?
        .step(request(endpoint))
        .await
        .err()
        .ok_or("expected invalid JSON failure")?;
    assert_eq!(error.code, "provider_response_invalid");
    assert!(!serde_json::to_string(&error)?.contains("private-invalid-json"));
    task.await??;
    Ok(())
}

#[tokio::test]
async fn body_deadline_is_enforced_and_redacted() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/private-endpoint", listener.local_addr()?);
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        capture_request(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
            .await?;
        held.await?;
        Ok::<_, TestError>(())
    });
    // Exercise the production deadline behavior without a 60-second test.
    let provider = HttpProvider {
        client: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()?,
    };
    let result =
        tokio::time::timeout(Duration::from_secs(5), provider.step(request(endpoint))).await?;
    let _ = release.send(());
    task.await??;
    let error = result.err().ok_or("expected provider timeout")?;
    assert_eq!(error.code, "provider_timeout");
    assert!(!serde_json::to_string(&error)?.contains("private-endpoint"));
    Ok(())
}

#[tokio::test]
async fn invalid_config_credentials_and_definitions_fail_before_http() -> TestResult {
    let provider = HttpProvider::new()?;
    for endpoint in [
        "not a URL",
        "file:///private",
        "https://",
        "http://user:password@127.0.0.1/",
        "http://user@127.0.0.1/",
        "http://127.0.0.1/#fragment",
    ] {
        assert_eq!(
            error_code(provider.step(request(endpoint.into())).await)?,
            "invalid_config"
        );
    }
    for secret in [
        "",
        "private\ncredential",
        "private credential",
        "private\tcredential",
        "日本語",
        "private\u{7f}credential",
    ] {
        let mut input = request("http://127.0.0.1:1/".into());
        input.credential = Some(secret.into());
        let error = provider
            .step(input)
            .await
            .err()
            .ok_or("expected invalid credential")?;
        assert_eq!(error.code, "invalid_secret");
        assert!(!serde_json::to_string(&error)?.contains("private"));
    }
    let mut invalid_name = tool();
    invalid_name.name = "not a function".into();
    let mut invalid_schema = tool();
    invalid_schema.input_schema = Value::Null;
    for definitions in [
        vec![tool(), tool()],
        vec![invalid_name],
        vec![invalid_schema],
    ] {
        let mut input = request("http://127.0.0.1:1/".into());
        input.tools = definitions;
        assert_eq!(error_code(provider.step(input).await)?, "invalid_params");
    }
    Ok(())
}

#[test]
fn validate_checks_options_without_discovery_and_tolerates_unknown_fields() -> TestResult {
    for value in [
        Value::Null,
        json!([]),
        json!(["http://localhost/", "mock"]),
        json!({}),
        json!({"model_endpoint": "http://localhost/", "model": 7}),
    ] {
        assert_eq!(error_code(validate(value))?, "invalid_config");
    }
    for model in [String::new(), " \n\t".into(), "m".repeat(257)] {
        assert_eq!(
            error_code(validate(
                json!({"model_endpoint": "http://localhost/", "model": model})
            ))?,
            "invalid_config"
        );
    }
    validate(
        json!({"model_endpoint": "https://example.invalid/custom?query", "model": "m".repeat(256), "future": true}),
    )?;
    Ok(())
}

#[test]
fn unsupported_finish_reasons_and_native_features_are_not_success() -> TestResult {
    let names = HashSet::from(["read_file"]);
    for finish in [
        "length",
        "content_filter",
        "function_call",
        "web_search",
        "unknown",
    ] {
        assert_eq!(
            error_code(decode_response(
                completion(json!({"role": "assistant", "content": "partial"}), finish),
                &names
            ))?,
            "provider_unsupported"
        );
    }
    for (key, value) in [
        ("refusal", json!("refused")),
        ("audio", json!({"id": "audio"})),
        ("annotations", json!([{"type": "url_citation"}])),
        ("function_call", json!({"name": "legacy"})),
        ("reasoning_details", json!([{"text": "private reasoning"}])),
        (
            "content",
            json!([{"type": "text", "text": "not supported"}]),
        ),
    ] {
        let mut message = json!({"role": "assistant", "content": "text"});
        message[key] = value;
        assert_eq!(
            error_code(decode_response(completion(message, "stop"), &names))?,
            "provider_unsupported"
        );
    }
    let mut message = tool_message();
    message["tool_calls"][0]["type"] = json!("custom");
    assert_eq!(
        error_code(decode_response(completion(message, "tool_calls"), &names))?,
        "provider_unsupported"
    );
    Ok(())
}

#[test]
fn malformed_shapes_and_host_calls_are_rejected() -> TestResult {
    let names = HashSet::from(["read_file"]);
    for value in [
        json!(null),
        json!({"choices": []}),
        json!({"choices": [{}, {}]}),
        completion(json!({"role": "user", "content": "wrong role"}), "stop"),
        completion(json!({"role": "assistant", "content": null}), "stop"),
        completion(tool_message(), "stop"),
        completion(json!({"role": "assistant", "tool_calls": []}), "tool_calls"),
    ] {
        assert_eq!(
            error_code(decode_response(value, &names))?,
            "provider_response_invalid"
        );
    }
    for arguments in [
        json!("not-json"),
        json!("[]"),
        json!("null"),
        json!({"path": "not serialized"}),
    ] {
        let mut message = tool_message();
        message["tool_calls"][0]["function"]["arguments"] = arguments;
        assert_eq!(
            error_code(decode_response(completion(message, "tool_calls"), &names))?,
            "provider_response_invalid"
        );
    }
    let mut message = tool_message();
    message["tool_calls"][0]["function"]["name"] = json!("shell");
    assert_eq!(
        error_code(decode_response(completion(message, "tool_calls"), &names))?,
        "provider_response_invalid"
    );
    let mut message = tool_message();
    let duplicate = message["tool_calls"][0].clone();
    message["tool_calls"]
        .as_array_mut()
        .ok_or("missing calls")?
        .push(duplicate);
    assert_eq!(
        error_code(decode_response(completion(message, "tool_calls"), &names))?,
        "provider_response_invalid"
    );
    Ok(())
}

#[tokio::test]
async fn normalized_history_replays_metadata_and_encodes_structured_tool_output() -> TestResult {
    let mut raw = tool_message();
    raw["annotations"] = json!([]);
    raw["refusal"] = Value::Null;
    raw["tool_calls"][0]["function"]["arguments"] = json!("{ \"path\" : \"notes.txt\" }");
    let (text, calls) = decode_message(&raw)?;
    let replay = ModelMessage::Assistant {
        text,
        tool_calls: calls,
        metadata: Some(ProviderMetadata {
            format: MESSAGE_FORMAT.into(),
            value: raw.clone(),
        }),
    };
    let output = json!({"content": "private\n日本語", "ok": true});
    let (endpoint, task) = mock_http(
        "200 OK",
        serde_json::to_vec(&completion(
            json!({"role": "assistant", "content": ""}),
            "stop",
        ))?,
        false,
        "",
    )
    .await?;
    let mut input = request(endpoint);
    input.messages = vec![
        ModelMessage::User {
            text: "input".into(),
        },
        replay,
        ModelMessage::ToolResult {
            call_id: "call-1".into(),
            output: output.clone(),
        },
        ModelMessage::Assistant {
            text: Some("previous text".into()),
            tool_calls: vec![],
            metadata: None,
        },
    ];
    // Historical calls remain replayable even if the tool is no longer advertised.
    assert!(
        matches!(HttpProvider::new()?.step(input).await?, ModelStep::Completed { text, .. } if text.is_empty())
    );
    let request = task.await??;
    assert_eq!(
        request.body["messages"],
        json!([
            {"role": "user", "content": "input"},
            raw,
            {"role": "tool", "tool_call_id": "call-1", "content": serde_json::to_string(&output)?},
            {"role": "assistant", "content": "previous text"},
        ])
    );
    Ok(())
}

#[test]
fn absent_metadata_reconstructs_supported_function_calls() -> TestResult {
    let raw = tool_message();
    let (text, calls) = decode_message(&raw)?;
    let message = ModelMessage::Assistant {
        text,
        tool_calls: calls,
        metadata: None,
    };
    assert_eq!(encode_message(&message)?, raw);
    for output in [json!("text"), json!(null), json!([1, 2]), json!(false)] {
        let encoded = encode_message(&ModelMessage::ToolResult {
            call_id: "id".into(),
            output: output.clone(),
        })?;
        assert_eq!(encoded["content"], serde_json::to_string(&output)?);
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_or_mismatched_replay_is_rejected_before_http() -> TestResult {
    let raw = tool_message();
    let (text, calls) = decode_message(&raw)?;
    let message = ModelMessage::Assistant {
        text,
        tool_calls: calls,
        metadata: Some(ProviderMetadata {
            format: MESSAGE_FORMAT.into(),
            value: raw,
        }),
    };
    let provider = HttpProvider::new()?;
    for variation in 0..7 {
        let mut message = message.clone();
        if let ModelMessage::Assistant {
            text,
            tool_calls,
            metadata: Some(metadata),
        } = &mut message
        {
            match variation {
                0 => metadata.format = "another.provider.format".into(),
                1 => *text = Some("mismatched private text".into()),
                2 => tool_calls[0].id = "different".into(),
                3 => tool_calls[0].name = "different".into(),
                4 => tool_calls[0].arguments = json!({"path": "different"}),
                5 => metadata.value["reasoning_details"] = json!([{"text": "private reasoning"}]),
                _ => metadata.value = Value::Null,
            }
        }
        let mut input = request("http://127.0.0.1:1/".into());
        input.messages.push(message);
        let error = provider
            .step(input)
            .await
            .err()
            .ok_or("expected replay rejection")?;
        assert_eq!(
            error.code,
            if matches!(variation, 0 | 5) {
                "provider_unsupported"
            } else {
                "provider_response_invalid"
            }
        );
        assert!(!serde_json::to_string(&error)?.contains("private"));
    }
    Ok(())
}

#[test]
fn invalid_semantic_assistant_without_metadata_is_not_reconstructed() -> TestResult {
    let (_, calls) = decode_message(&tool_message())?;
    for variation in 0..5 {
        let mut calls = calls.clone();
        match variation {
            0 => calls.clear(),
            1 => calls[0].arguments = json!("not an object"),
            2 => calls[0].id.clear(),
            3 => calls[0].name = "not a function".into(),
            _ => calls.push(calls[0].clone()),
        }
        assert_eq!(
            error_code(encode_message(&ModelMessage::Assistant {
                text: None,
                tool_calls: calls,
                metadata: None
            }))?,
            "provider_response_invalid"
        );
    }
    Ok(())
}
