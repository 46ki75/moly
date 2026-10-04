use super::*;
use std::error::Error as StdError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

// These fixtures validate the adapter's documented wire contract, not live SIWC
// access, account entitlements, OAuth, or upstream acceptance of a model/schema.
type TestError = Box<dyn StdError + Send + Sync>;
type TestResult = Result<(), TestError>;

fn request() -> Result<ModelRequest, TestError> {
    Ok(serde_json::from_value(json!({
        "options": {"temperature": 1, "previous_response_id": "must-not-send"},
        "credential": "must-not-send",
        "context": {
            "session_id": "00000000-0000-4000-8000-000000000001",
            "run_id": "00000000-0000-4000-8000-000000000002",
            "model_call_id": "00000000-0000-4000-8000-000000000003",
            "call_kind": "primary"
        },
        "messages": [{"kind": "user", "text": "hello"}],
        "tools": [{
            "name": "read_file", "description": "Read a workspace file",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
        }]
    }))?)
}

fn message(id: &str, text: &str, phase: &str) -> Value {
    json!({
        "id": id, "type": "message", "role": "assistant", "status": "completed",
        "phase": phase,
        "content": [{"type": "output_text", "text": text, "annotations": [], "logprobs": []}]
    })
}

fn function(id: &str) -> Value {
    json!({
        "id": format!("fc-{id}"), "type": "function_call", "call_id": id,
        "name": "read_file", "namespace": "moly", "status": "completed",
        "arguments": "{\"path\": \"notes.txt\"}", "caller": {"type": "direct"}, "async": false
    })
}

fn reasoning() -> Value {
    json!({
        "id": "rs-1", "type": "reasoning", "status": "completed",
        "summary": [{"type": "summary_text", "text": "opaque summary"}],
        "content": [{"type": "reasoning_text", "text": "opaque reasoning"}],
        "encrypted_content": "opaque-completed-ciphertext"
    })
}

fn completed(output: Value) -> Value {
    json!({"type": "response.completed", "response": {
        "id": "resp-1", "object": "response", "status": "completed", "output": output,
        "error": null, "incomplete_details": null
    }})
}

fn wire(value: &Value) -> Result<Vec<u8>, TestError> {
    Ok(format!(
        "event: {}\ndata: {}\n\n",
        string(value, "type")?,
        serde_json::to_string(value)?
    )
    .into_bytes())
}

fn decode(value: Value) -> Result<ModelStep, Error> {
    let mut state = ResponseState::default();
    let data = serde_json::to_string(&value).map_err(|_| Error::Internal)?;
    state.accept(&data, None, &HashSet::from(["read_file"]))?;
    state.completed.ok_or(Error::Interrupted)
}

fn assert_error<T>(result: Result<T, Error>, expected: Error) -> TestResult {
    match result {
        Ok(_) => Err("expected a typed adapter failure".into()),
        Err(actual) => {
            assert_eq!(
                std::mem::discriminant(&actual),
                std::mem::discriminant(&expected)
            );
            Ok(())
        }
    }
}

fn assistant(step: ModelStep) -> ModelMessage {
    match step {
        ModelStep::Completed { text, metadata } => ModelMessage::Assistant {
            text: Some(text),
            tool_calls: Vec::new(),
            metadata,
        },
        ModelStep::AwaitHostTools {
            text,
            calls,
            metadata,
        } => ModelMessage::Assistant {
            text,
            tool_calls: calls,
            metadata,
        },
    }
}

#[test]
fn terminal_completion_is_independent_of_trailing_bytes_and_http_chunks() -> TestResult {
    let terminal = wire(&completed(json!([message(
        "msg-1",
        "done",
        "final_answer"
    )])))?;
    for trailer in [b"data: [DONE]\n\n".as_slice(), b"data: \xff\n\n"] {
        let mut bytes = terminal.clone();
        bytes.extend_from_slice(trailer);
        for split in 0..=bytes.len() {
            let mut parser = SseParser::default();
            let mut state = ResponseState::default();
            for chunk in [&bytes[..split], &bytes[split..]] {
                if state.completed.is_some() {
                    break;
                }
                parser.feed(chunk, |data, event| {
                    state.accept(data, event, &HashSet::new())
                })?;
            }
            assert!(
                matches!(state.completed, Some(ModelStep::Completed { text, .. }) if text == "done")
            );
        }
    }
    Ok(())
}

#[test]
fn removing_current_tools_does_not_remove_historical_calls() -> TestResult {
    for with_metadata in [true, false] {
        let mut input = request()?;
        let mut historical = assistant(decode(completed(json!([function("call-1")])))?);
        if let ModelMessage::Assistant { metadata, .. } = &mut historical
            && !with_metadata
        {
            *metadata = None;
        }
        input.messages.extend([
            historical,
            ModelMessage::ToolResult {
                call_id: "call-1".into(),
                output: json!({"text":"old result"}),
            },
            ModelMessage::User {
                text: "Continue without tools".into(),
            },
        ]);
        input.tools.clear();
        let body = encode(&input, "selected-model")?;
        assert!(body.get("tools").is_none());
        assert_eq!(body["input"][1]["name"], "read_file");
        assert_eq!(body["input"][2]["call_id"], "call-1");
    }
    Ok(())
}

#[test]
fn exact_supported_body_and_namespace_shape() -> TestResult {
    let mut input = request()?;
    let body = encode(&input, "selected-model")?;
    assert_eq!(
        body,
        json!({
            "model": "selected-model", "stream": true, "store": false,
            "include": ["reasoning.encrypted_content"],
            "input": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "namespace", "name": "moly",
                "description": "Server-advertised hosted capabilities",
                "tools": [{"type": "function", "name": "read_file", "description": "Read a workspace file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}, "strict": false}]
            }]
        })
    );
    input.tools.clear();
    assert!(encode(&input, "selected-model")?.get("tools").is_none());
    for model in ["", "  ", "bad\nmodel"] {
        assert_error(encode(&input, model), Error::InvalidConfig)?;
    }
    assert_error(encode(&input, &"x".repeat(257)), Error::InvalidConfig)?;
    Ok(())
}

#[test]
fn replay_preserves_raw_phase_reasoning_and_argument_spelling() -> TestResult {
    let output = json!([
        reasoning(),
        message("msg-commentary", "Checking. ", "commentary"),
        function("call-1"),
        message("msg-final", "Ready.", "final_answer")
    ]);
    let step = decode(completed(output.clone()))?;
    let mut input = request()?;
    match &step {
        ModelStep::AwaitHostTools {
            text,
            calls,
            metadata,
        } => {
            assert_eq!(text.as_deref(), Some("Checking. Ready."));
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "call-1");
            assert_eq!(calls[0].name, "read_file");
            assert_eq!(calls[0].arguments, json!({"path": "notes.txt"}));
            let metadata = metadata.as_ref().ok_or("missing metadata")?;
            assert_eq!(metadata.format, OUTPUT_FORMAT);
            assert_eq!(metadata.value, output);
        }
        _ => return Err("expected hosted-tool request".into()),
    }
    input.messages.push(assistant(step));
    input.messages.push(ModelMessage::ToolResult {
        call_id: "call-1".into(),
        output: json!({"error": {"code": "tool_file_not_found"}}),
    });
    let body = encode(&input, "selected-model")?;
    let items = body["input"].as_array().ok_or("input must be an array")?;
    assert_eq!(
        &items[1..5],
        output.as_array().ok_or("missing output")?.as_slice()
    );
    assert_eq!(
        items[5],
        json!({
            "type": "function_call_output", "call_id": "call-1", "name": "read_file", "namespace": "moly",
            "output": "{\"error\":{\"code\":\"tool_file_not_found\"}}"
        })
    );
    Ok(())
}

#[test]
fn missing_metadata_reconstructs_only_normalized_supported_items() -> TestResult {
    let mut input = request()?;
    input.messages.push(ModelMessage::Assistant {
        text: Some("".into()),
        tool_calls: vec![HostToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "a"}),
        }],
        metadata: None,
    });
    input.messages.push(ModelMessage::ToolResult {
        call_id: "call-1".into(),
        output: json!([1, true]),
    });
    let body = encode(&input, "selected-model")?;
    assert_eq!(
        body["input"][1],
        json!({"role": "assistant", "content": ""})
    );
    assert_eq!(
        body["input"][2],
        json!({
            "type": "function_call", "call_id": "call-1", "name": "read_file", "namespace": "moly", "arguments": "{\"path\":\"a\"}"
        })
    );
    assert_eq!(body["input"][3]["output"], "[1,true]");
    Ok(())
}

#[test]
fn inconsistent_or_foreign_replay_fails_closed() -> TestResult {
    let mut base = request()?;
    base.messages.push(assistant(decode(completed(json!([
        message("msg-1", "Checking.", "commentary"),
        function("call-1")
    ])))?));
    base.messages.push(ModelMessage::ToolResult {
        call_id: "call-1".into(),
        output: json!(null),
    });
    for index in 0..10 {
        let mut input = base.clone();
        let ModelMessage::Assistant {
            text,
            tool_calls,
            metadata,
        } = &mut input.messages[1]
        else {
            return Err("bad test fixture".into());
        };
        let metadata = metadata.as_mut().ok_or("missing fixture metadata")?;
        match index {
            0 => metadata.format = "foreign.format".into(),
            1 => metadata.value = json!({"output": []}),
            2 => *text = Some("different".into()),
            3 => *text = None,
            4 => tool_calls[0].id = "different".into(),
            5 => tool_calls[0].name = "different".into(),
            6 => tool_calls[0].arguments = json!({"path": "different"}),
            7 => metadata.value[1]["namespace"] = json!("foreign"),
            8 => metadata.value[0]["role"] = json!("system"),
            _ => metadata.value[0]["phase"] = json!("unknown"),
        }
        assert_error(encode(&input, "selected-model"), Error::Replay)?;
    }
    Ok(())
}

#[test]
fn invalid_tools_and_history_correlations_are_rejected() -> TestResult {
    for index in 0..4 {
        let mut input = request()?;
        match index {
            0 => input.tools[0].name.clear(),
            1 => input.tools[0].name = "moly.read_file".into(),
            2 => input.tools[0].input_schema = json!([]),
            _ => input.tools.push(input.tools[0].clone()),
        }
        assert_error(encode(&input, "selected-model"), Error::InvalidParams)?;
    }
    let call = HostToolCall {
        id: "call-1".into(),
        name: "read_file".into(),
        arguments: json!({}),
    };
    let response = ModelMessage::Assistant {
        text: None,
        tool_calls: vec![call.clone()],
        metadata: None,
    };
    let result = ModelMessage::ToolResult {
        call_id: "call-1".into(),
        output: json!({}),
    };
    for messages in [
        vec![result.clone()],
        vec![response.clone()],
        vec![response.clone(), result.clone(), result.clone()],
        vec![
            response.clone(),
            ModelMessage::User {
                text: "interrupt".into(),
            },
            result.clone(),
        ],
        vec![response.clone(), response.clone(), result.clone()],
        vec![
            response.clone(),
            result.clone(),
            response.clone(),
            result.clone(),
        ],
        vec![ModelMessage::Assistant {
            text: None,
            tool_calls: vec![],
            metadata: None,
        }],
        vec![
            ModelMessage::Assistant {
                text: None,
                tool_calls: vec![call.clone(), call],
                metadata: None,
            },
            result,
        ],
    ] {
        let mut input = request()?;
        input.messages = messages;
        assert_error(encode(&input, "selected-model"), Error::Replay)?;
    }
    Ok(())
}

#[test]
fn empty_or_reasoning_only_replay_is_not_an_assistant_outcome() -> TestResult {
    for output in [json!([]), json!([reasoning()])] {
        let mut input = request()?;
        input.messages.push(ModelMessage::Assistant {
            text: None,
            tool_calls: Vec::new(),
            metadata: Some(ProviderMetadata {
                format: OUTPUT_FORMAT.into(),
                value: output,
            }),
        });
        assert_error(encode(&input, "selected-model"), Error::Replay)?;
    }
    Ok(())
}

#[test]
fn malformed_duplicate_and_unadvertised_function_calls_are_rejected() -> TestResult {
    for (field, value, expected) in [
        ("call_id", json!(""), Error::InvalidResponse),
        ("call_id", json!("\n"), Error::InvalidResponse),
        ("arguments", json!("not-json"), Error::InvalidResponse),
        ("arguments", json!("[]"), Error::InvalidResponse),
        ("arguments", json!({}), Error::InvalidResponse),
        ("name", json!("not_advertised"), Error::Unsupported),
        ("name", json!("moly.read_file"), Error::Unsupported),
        ("namespace", json!("foreign"), Error::Unsupported),
        ("async", json!(true), Error::Unsupported),
        ("async", json!("false"), Error::InvalidResponse),
        (
            "caller",
            json!({"type": "program", "caller_id": "program-1"}),
            Error::Unsupported,
        ),
        ("status", json!("incomplete"), Error::Incomplete),
        ("id", json!(null), Error::InvalidResponse),
    ] {
        let mut item = function("call-1");
        item[field] = value;
        assert_error(decode(completed(json!([item]))), expected)?;
    }
    for field in ["call_id", "name", "arguments", "namespace"] {
        let mut item = function("call-1");
        item.as_object_mut().ok_or("bad fixture")?.remove(field);
        assert_error(decode(completed(json!([item]))), Error::InvalidResponse)?;
    }
    let mut second = function("call-1");
    second["id"] = json!("different-item-id");
    assert_error(
        decode(completed(json!([function("call-1"), second]))),
        Error::InvalidResponse,
    )?;
    let mut second = function("call-2");
    second["id"] = function("call-1")["id"].clone();
    assert_error(
        decode(completed(json!([function("call-1"), second]))),
        Error::InvalidResponse,
    )?;
    for count in [32, 33] {
        let calls: Vec<_> = (0..count)
            .map(|index| function(&format!("call-{index}")))
            .collect();
        if count == 32 {
            assert!(matches!(
                decode(completed(json!(calls)))?,
                ModelStep::AwaitHostTools { .. }
            ));
        } else {
            assert_error(decode(completed(json!(calls))), Error::InvalidResponse)?;
        }
    }
    Ok(())
}

#[test]
fn only_explicit_final_text_or_hosted_calls_can_complete() -> TestResult {
    match decode(completed(json!([message("msg-1", "", "final_answer")])))? {
        ModelStep::Completed { text, metadata } => {
            assert_eq!(text, "");
            assert_eq!(
                metadata.ok_or("missing metadata")?.value[0]["phase"],
                "final_answer"
            );
        }
        _ => return Err("expected explicit empty text".into()),
    }
    assert_error(decode(completed(json!([]))), Error::InvalidResponse)?;
    assert_error(
        decode(completed(json!([reasoning()]))),
        Error::InvalidResponse,
    )?;
    let mut item = message("msg-1", "hello", "final_answer");
    item["content"] = json!([]);
    assert_error(decode(completed(json!([item]))), Error::InvalidResponse)?;
    for kind in [
        "web_search_call",
        "computer_call",
        "file_search_call",
        "code_interpreter_call",
        "custom_tool_call",
        "mcp_call",
        "tool_search_call",
        "compaction",
    ] {
        assert_error(
            decode(completed(json!([{"id": "native-1", "type": kind}]))),
            Error::Unsupported,
        )?;
    }
    let mut item = message("msg-1", "hello", "final_answer");
    item["content"] = json!([{"type": "refusal", "refusal": "no"}]);
    assert_error(decode(completed(json!([item]))), Error::Unsupported)?;
    for field in ["error", "incomplete_details"] {
        let mut event = completed(json!([message("msg-1", "hello", "final_answer")]));
        event["response"][field] = json!({"reason": "failure"});
        assert_error(decode(event), Error::InvalidResponse)?;
    }
    Ok(())
}

#[test]
fn reasoning_done_supplies_final_ciphertext_and_conflicts_are_rejected() -> TestResult {
    let names = HashSet::from(["read_file"]);
    let done = json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning()});
    for conflict in [false, true] {
        let mut state = ResponseState::default();
        state.accept(&serde_json::to_string(&done)?, None, &names)?;
        let mut terminal_reasoning = reasoning();
        if conflict {
            terminal_reasoning["encrypted_content"] = json!("conflicting-ciphertext");
        } else {
            terminal_reasoning
                .as_object_mut()
                .ok_or("bad fixture")?
                .remove("encrypted_content");
        }
        let final_event = completed(json!([
            terminal_reasoning,
            message("msg-1", "hello", "final_answer")
        ]));
        let result = state.accept(&serde_json::to_string(&final_event)?, None, &names);
        if conflict {
            assert_error(result, Error::InvalidResponse)?;
        } else {
            result?;
            let ModelStep::Completed { metadata, .. } = state.completed.ok_or("no completion")?
            else {
                return Err("expected text".into());
            };
            assert_eq!(metadata.ok_or("missing metadata")?.value[0], reasoning());
        }
    }
    let mut state = ResponseState::default();
    state.accept(&serde_json::to_string(&done)?, None, &names)?;
    assert_error(
        state.accept(&serde_json::to_string(&done)?, None, &names),
        Error::InvalidResponse,
    )?;
    let mut state = ResponseState::default();
    let done = json!({"type": "response.output_item.done", "output_index": 0,
        "item": message("msg-1", "original", "final_answer")});
    state.accept(&serde_json::to_string(&done)?, None, &names)?;
    assert_error(
        state.accept(
            &serde_json::to_string(&completed(json!([message(
                "msg-1",
                "changed",
                "final_answer"
            )])))?,
            None,
            &names,
        ),
        Error::InvalidResponse,
    )?;
    Ok(())
}

#[test]
fn sse_framing_handles_arbitrary_splits_utf8_bom_comments_and_multiline() -> TestResult {
    let terminal = completed(json!([message("msg-1", "Hello, 🌍!", "final_answer")]));
    let pretty = serde_json::to_string_pretty(&terminal)?;
    let mut bytes =
        String::from("\u{feff}: comment\r\nid: ignored\rretry: 0\nevent: response.completed\r\n")
            .into_bytes();
    for line in pretty.lines() {
        bytes.extend_from_slice(format!("data: {line}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"\r\n");
    for chunk_size in [1, 2, 3, 7, 4096] {
        let mut parser = SseParser::default();
        let mut state = ResponseState::default();
        for chunk in bytes.chunks(chunk_size) {
            parser.feed(chunk, |data, event| {
                state.accept(data, event, &HashSet::from(["read_file"]))
            })?;
        }
        match state.completed.ok_or("missing SSE completion")? {
            ModelStep::Completed { text, .. } => assert_eq!(text, "Hello, 🌍!"),
            _ => return Err("expected text completion".into()),
        }
    }
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    parser.feed(b": heartbeat\n\ndata: one\n\ndata: two\n\n", |data, _| {
        events.push(data.to_owned());
        Ok(false)
    })?;
    assert_eq!(events, ["one", "two"]);
    Ok(())
}

#[test]
fn malformed_sse_and_missing_terminal_do_not_succeed() -> TestResult {
    for (bytes, expected) in [
        (b"data: not-json\n\n".as_slice(), Error::InvalidResponse),
        (b"data: \xff\n\n".as_slice(), Error::InvalidResponse),
        (b"data: [DONE]\n\n".as_slice(), Error::Interrupted),
        (b"event: response.completed\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n".as_slice(), Error::InvalidResponse),
        (b"data: {\"type\":\"response.unknown\"}\n\n".as_slice(), Error::Unsupported),
    ] {
        let mut parser = SseParser::default();
        let mut state = ResponseState::default();
        assert_error(parser.feed(bytes, |data, event| state.accept(data, event, &HashSet::new())), expected)?;
        assert!(state.completed.is_none());
    }
    let mut bytes = wire(&completed(json!([message(
        "msg-1",
        "hello",
        "final_answer"
    )])))?;
    bytes.pop();
    let mut parser = SseParser::default();
    let mut state = ResponseState::default();
    parser.feed(&bytes, |data, event| {
        state.accept(data, event, &HashSet::new())
    })?;
    assert!(state.completed.is_none());
    Ok(())
}

#[test]
fn response_identity_sequence_and_terminal_status_are_checked() -> TestResult {
    let names = HashSet::new();
    let created = json!({"type": "response.created", "sequence_number": 0,
        "response": {"id": "resp-1", "status": "in_progress"}});
    for index in 0..4 {
        let mut state = ResponseState::default();
        state.accept(&serde_json::to_string(&created)?, None, &names)?;
        let mut event = completed(json!([message("msg-1", "hello", "final_answer")]));
        event["sequence_number"] = json!(1);
        match index {
            0 => event["response"]["id"] = json!("resp-different"),
            1 => event["sequence_number"] = json!(0),
            2 => event["sequence_number"] = json!(-1),
            _ => event["response"]["status"] = json!("incomplete"),
        }
        assert_error(
            state.accept(&serde_json::to_string(&event)?, None, &names),
            Error::InvalidResponse,
        )?;
    }
    Ok(())
}

#[test]
fn byte_limits_include_comments_replay_and_the_normalized_step() -> TestResult {
    let mut parser = SseParser::default();
    let exact = format!(":{}\n\n", "x".repeat(MAX_EVENT_BYTES - 3));
    parser.feed(exact.as_bytes(), |_, _| Err(Error::Internal))?;
    let mut parser = SseParser::default();
    let oversized = format!(":{}\n\n", "x".repeat(MAX_EVENT_BYTES - 2));
    assert_error(
        parser.feed(oversized.as_bytes(), |_, _| Ok(false)),
        Error::TooLarge,
    )?;
    let mut parser = SseParser::default();
    for _ in 0..4 {
        parser.feed(exact.as_bytes(), |_, _| Ok(false))?;
    }
    assert_error(parser.feed(b"x", |_, _| Ok(false)), Error::TooLarge)?;
    let mut parser = SseParser::default();
    let exact_crlf = format!(":{}\r\n\r\n", "x".repeat(MAX_EVENT_BYTES - 5));
    for chunk in exact_crlf.as_bytes().chunks(4093) {
        parser.feed(chunk, |_, _| Ok(false))?;
    }
    let mut parser = SseParser::default();
    let oversized_crlf = format!(":{}\r\n\r\n", "x".repeat(MAX_EVENT_BYTES - 4));
    assert_error(
        parser.feed(oversized_crlf.as_bytes(), |_, _| Ok(false)),
        Error::TooLarge,
    )?;
    let output = json!([message(
        "msg-1",
        &"x".repeat(MAX_OUTPUT_BYTES / 2),
        "final_answer"
    )]);
    assert_error(decode(completed(output)), Error::TooLarge)?;
    let mut input = request()?;
    input.messages.push(ModelMessage::Assistant {
        text: Some("x".repeat(MAX_OUTPUT_BYTES)),
        tool_calls: Vec::new(),
        metadata: Some(ProviderMetadata {
            format: OUTPUT_FORMAT.into(),
            value: json!([message(
                "msg-1",
                &"x".repeat(MAX_OUTPUT_BYTES),
                "final_answer"
            )]),
        }),
    });
    assert_error(encode(&input, "selected-model"), Error::TooLarge)?;
    let mut input = request()?;
    input.messages = vec![ModelMessage::User {
        text: "x".repeat(MAX_BODY_BYTES),
    }];
    assert_error(encode(&input, "selected-model"), Error::TooLarge)?;
    assert!(ensure_size(&json!("abc"), 5).is_ok());
    assert_error(ensure_size(&json!("abc"), 4), Error::TooLarge)?;
    Ok(())
}

struct CapturedRequest {
    headers: String,
    body: Value,
}

type MockTask = JoinHandle<Result<CapturedRequest, TestError>>;

async fn capture_request(stream: &mut TcpStream) -> Result<CapturedRequest, TestError> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 || count > MAX_BODY_BYTES - bytes.len() {
            return Err("invalid mock HTTP request".into());
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
        .ok_or("missing request length")?
        .1
        .trim()
        .parse()?;
    if length > MAX_BODY_BYTES - header_end {
        return Err("oversized mock HTTP request".into());
    }
    while bytes.len() < header_end + length {
        let count = stream.read(&mut buffer).await?;
        if count == 0 || count > MAX_BODY_BYTES - bytes.len() {
            return Err("truncated mock HTTP request".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(CapturedRequest {
        headers,
        body: serde_json::from_slice(&bytes[header_end..header_end + length])?,
    })
}

async fn mock_http(
    status: &str,
    content_type: &str,
    body: Vec<u8>,
    chunk_size: usize,
    finish: bool,
    extra: &str,
) -> Result<(String, MockTask), TestError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/responses", listener.local_addr()?);
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n{extra}\r\n"
    );
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let (mut stream, _) = listener.accept().await?;
            let request = capture_request(&mut stream).await?;
            // A rejecting client may close during writes; that is expected.
            if stream.write_all(headers.as_bytes()).await.is_ok() {
                for chunk in body.chunks(chunk_size) {
                    if stream
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .await
                        .is_err()
                        || stream.write_all(chunk).await.is_err()
                        || stream.write_all(b"\r\n").await.is_err()
                    {
                        break;
                    }
                }
                if finish {
                    let _ = stream.write_all(b"0\r\n\r\n").await;
                }
            }
            Ok::<_, TestError>(request)
        })
        .await?
    });
    Ok((endpoint, task))
}

fn client() -> Result<reqwest::Client, TestError> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()?)
}

async fn assert_peer_closed(stream: &mut TcpStream) -> TestResult {
    let mut byte = [0; 1];
    match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await? {
        Ok(0) => Ok(()),
        // Dropping an unread HTTP body may reset rather than gracefully FIN.
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err("unexpected data instead of peer closure".into()),
    }
}

#[tokio::test]
async fn http_sse_completion_uses_bearer_and_exact_body_once() -> TestResult {
    let output = json!([reasoning(), message("msg-1", "Hello, 🌍!", "final_answer")]);
    let mut body =
        wire(&json!({"type": "response.output_text.delta", "delta": "not authoritative"}))?;
    body.extend(wire(&completed(output.clone()))?);
    let (endpoint, task) = mock_http(
        "200 OK",
        "text/event-stream; charset=utf-8",
        body,
        3,
        true,
        "",
    )
    .await?;
    let input = request()?;
    let step = infer(
        &client()?,
        &endpoint,
        "test-access-token",
        &input,
        "selected-model",
    )
    .await?;
    match step {
        ModelStep::Completed { text, metadata } => {
            assert_eq!(text, "Hello, 🌍!");
            assert_eq!(metadata.ok_or("no metadata")?.value, output);
        }
        _ => return Err("expected text completion".into()),
    }
    let captured = task.await??;
    let headers = captured.headers.to_ascii_lowercase();
    assert!(headers.starts_with("post /v1/responses http/1.1\r\n"));
    assert!(headers.contains("authorization: bearer test-access-token\r\n"));
    assert!(headers.contains("accept: text/event-stream\r\n"));
    assert_eq!(captured.body, encode(&input, "selected-model")?);
    Ok(())
}

#[tokio::test]
async fn http_tool_batch_round_trip_retains_reasoning_and_both_calls() -> TestResult {
    let output = json!([reasoning(), function("call-1"), function("call-2")]);
    let mut body = wire(
        &json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning()}),
    )?;
    body.extend(wire(&completed(output.clone()))?);
    let (endpoint, task) = mock_http("200 OK", "text/event-stream", body, 7, true, "").await?;
    let mut input = request()?;
    let step = infer(
        &client()?,
        &endpoint,
        "test-access-token",
        &input,
        "selected-model",
    )
    .await?;
    let ModelStep::AwaitHostTools {
        text,
        calls,
        metadata,
    } = &step
    else {
        return Err("expected hosted tools".into());
    };
    assert!(text.is_none());
    assert_eq!(calls.len(), 2);
    assert_eq!(metadata.as_ref().ok_or("missing metadata")?.value, output);
    task.await??;
    input.messages.push(assistant(step));
    for id in ["call-1", "call-2"] {
        input.messages.push(ModelMessage::ToolResult {
            call_id: id.into(),
            output: json!({"content": "done"}),
        });
    }
    let body = wire(&completed(json!([message(
        "msg-2",
        "Finished.",
        "final_answer"
    )])))?;
    let (endpoint, task) = mock_http("200 OK", "text/event-stream", body, 4096, true, "").await?;
    assert!(matches!(
        infer(
            &client()?,
            &endpoint,
            "test-access-token",
            &input,
            "selected-model"
        )
        .await?,
        ModelStep::Completed { .. }
    ));
    let captured = task.await??;
    assert_eq!(
        &captured.body["input"].as_array().ok_or("missing input")?[1..4],
        output.as_array().ok_or("bad fixture")?.as_slice()
    );
    Ok(())
}

#[tokio::test]
async fn http_stream_failures_and_usage_errors_never_return_partial_success() -> TestResult {
    for (event, expected) in [
        (
            json!({"type": "response.failed", "response": {"id": "resp-1", "error": {"code": "subscription_sharing_usage_limit_exceeded"}}}),
            Error::UsageLimit,
        ),
        (
            json!({"type": "response.failed", "response": {"id": "resp-1", "error": {"code": "subscription_sharing_usage_unavailable"}}}),
            Error::UsageUnavailable,
        ),
        (
            json!({"type": "response.failed", "response": {"id": "resp-1", "error": {"code": "unknown"}}}),
            Error::ProviderFailed,
        ),
        (
            json!({"type": "response.incomplete", "response": {"id": "resp-1", "status": "incomplete"}}),
            Error::Incomplete,
        ),
        (
            json!({"type": "error", "code": "subscription_sharing_unsupported_capability", "message": "do not expose"}),
            Error::Unsupported,
        ),
        (
            json!({"type": "response.output_item.added", "item": {"type": "computer_call"}}),
            Error::Unsupported,
        ),
        (
            completed(json!([function("call-1"), function("call-1")])),
            Error::InvalidResponse,
        ),
    ] {
        let mut body =
            wire(&json!({"type": "response.output_text.delta", "delta": "partial text"}))?;
        body.extend(wire(&event)?);
        let (endpoint, task) =
            mock_http("200 OK", "text/event-stream", body, 4096, true, "").await?;
        assert_error(
            infer(
                &client()?,
                &endpoint,
                "test-access-token",
                &request()?,
                "selected-model",
            )
            .await,
            expected,
        )?;
        task.await??;
    }
    Ok(())
}

#[tokio::test]
async fn http_disconnect_eof_and_partial_terminal_are_interrupted_without_retry() -> TestResult {
    let delta = wire(&json!({"type": "response.output_text.delta", "delta": "partial"}))?;
    let mut partial = wire(&completed(json!([message(
        "msg-1",
        "hello",
        "final_answer"
    )])))?;
    partial.pop();
    for (body, finish) in [
        (delta.clone(), true),
        (delta, false),
        (partial, true),
        (Vec::new(), true),
    ] {
        let (endpoint, task) =
            mock_http("200 OK", "text/event-stream", body, 4096, finish, "").await?;
        assert_error(
            infer(
                &client()?,
                &endpoint,
                "test-access-token",
                &request()?,
                "selected-model",
            )
            .await,
            Error::Interrupted,
        )?;
        task.await??;
    }
    Ok(())
}

#[tokio::test]
async fn http_admission_status_and_structured_codes_are_sanitized() -> TestResult {
    for (status, body, expected) in [
        (
            "401 Unauthorized",
            json!({"detail": "private admission diagnostic"}),
            Error::AuthRequired,
        ),
        (
            "403 Forbidden",
            json!({"detail": "private admission diagnostic"}),
            Error::PermissionDenied,
        ),
        (
            "429 Too Many Requests",
            json!({"error": {"code": "subscription_sharing_usage_limit_exceeded"}}),
            Error::UsageLimit,
        ),
        (
            "503 Service Unavailable",
            json!({"error": {"code": "subscription_sharing_usage_unavailable"}}),
            Error::UsageUnavailable,
        ),
        (
            "400 Bad Request",
            json!({"error": {"code": "subscription_sharing_unsupported_capability"}}),
            Error::Unsupported,
        ),
        (
            "403 Forbidden",
            json!({"error": {"code": "subscription_sharing_user_not_eligible"}}),
            Error::PermissionDenied,
        ),
        (
            "503 Service Unavailable",
            json!({"detail": "temporary routing failure"}),
            Error::ProviderFailed,
        ),
        (
            "500 Internal Server Error",
            json!(null),
            Error::ProviderFailed,
        ),
        ("408 Request Timeout", json!(null), Error::Timeout),
        ("413 Content Too Large", json!(null), Error::TooLarge),
        (
            "307 Temporary Redirect",
            json!(null),
            Error::InvalidResponse,
        ),
    ] {
        let (endpoint, task) = mock_http(
            status,
            "application/json",
            serde_json::to_vec(&body)?,
            4096,
            true,
            "",
        )
        .await?;
        assert_error(
            infer(
                &client()?,
                &endpoint,
                "test-access-token",
                &request()?,
                "selected-model",
            )
            .await,
            expected,
        )?;
        task.await??;
    }
    Ok(())
}

#[tokio::test]
async fn http_content_type_and_stream_body_bounds_are_enforced() -> TestResult {
    let body = wire(&completed(json!([message(
        "msg-1",
        "hello",
        "final_answer"
    )])))?;
    let (endpoint, task) = mock_http("200 OK", "application/json", body, 4096, true, "").await?;
    assert_error(
        infer(
            &client()?,
            &endpoint,
            "test-access-token",
            &request()?,
            "selected-model",
        )
        .await,
        Error::InvalidResponse,
    )?;
    task.await??;
    let event = format!(":{}\n\n", "x".repeat(MAX_EVENT_BYTES - 3)).into_bytes();
    let aggregate: Vec<_> = event
        .iter()
        .copied()
        .cycle()
        .take(MAX_BODY_BYTES + 1)
        .collect();
    for (status, content_type, body) in [
        (
            "200 OK",
            "text/event-stream",
            vec![b'x'; MAX_EVENT_BYTES + 1],
        ),
        ("200 OK", "text/event-stream", aggregate),
        (
            "500 Internal Server Error",
            "application/json",
            vec![b'x'; MAX_BODY_BYTES + 1],
        ),
    ] {
        let (endpoint, task) = mock_http(status, content_type, body, 4096, true, "").await?;
        assert_error(
            infer(
                &client()?,
                &endpoint,
                "test-access-token",
                &request()?,
                "selected-model",
            )
            .await,
            Error::TooLarge,
        )?;
        task.await??;
    }
    Ok(())
}

#[tokio::test]
async fn client_deadline_can_be_shorter_than_the_adapter_deadline() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/responses", listener.local_addr()?);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        capture_request(&mut stream).await?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await?;
        ready_tx.send(()).map_err(|_| "lost readiness receiver")?;
        assert_peer_closed(&mut stream).await
    });
    // Use a shorter read timeout, not a sleep or a 60-second test. infer's
    // request-level timeout intentionally overrides ClientBuilder::timeout.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .read_timeout(Duration::from_millis(100))
        .build()?;
    let input = request()?;
    let inference = infer(
        &client,
        &endpoint,
        "test-access-token",
        &input,
        "selected-model",
    );
    let (result, ready) = tokio::join!(inference, ready_rx);
    ready?;
    assert_error(result, Error::Timeout)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn dropping_inference_closes_the_pending_http_stream() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/responses", listener.local_addr()?);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        capture_request(&mut stream).await?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await?;
        ready_tx.send(()).map_err(|_| "lost readiness receiver")?;
        assert_peer_closed(&mut stream).await
    });
    let client = client()?;
    let input = request()?;
    {
        let inference = infer(
            &client,
            &endpoint,
            "test-access-token",
            &input,
            "selected-model",
        );
        tokio::pin!(inference);
        tokio::select! {
            result = &mut inference => { result?; return Err("inference finished before cancellation".into()); }
            ready = ready_rx => { ready?; }
        }
    }
    task.await??;
    Ok(())
}

#[test]
fn endpoint_injection_is_loopback_only_and_has_no_legacy_fallback() -> TestResult {
    validate_endpoint(ENDPOINT)?;
    validate_endpoint("http://127.0.0.1:1234/v1/responses")?;
    for endpoint in [
        "https://chatgpt.com/backend-api/codex/responses",
        "https://example.test/v1/responses",
        "http://127.0.0.1:1234/v1/responses#fragment",
        "http://user:secret@127.0.0.1:1234/v1/responses",
        "http://127.0.0.1:1234/v1/responses?redirect=external",
        "http://localhost:1234/v1/responses",
    ] {
        assert_error(validate_endpoint(endpoint), Error::InvalidConfig)?;
    }
    Ok(())
}
