//! Hermetic checks of the actual binary's stdio and process lifetime.

use std::process::Stdio;
use std::time::Duration;

use moly_protocol::{Body, Message};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), TestError>;

fn spawn() -> Result<(Child, ChildStdin, BufReader<ChildStdout>), TestError> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_moly-provider-openai"))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let input = child.stdin.take().ok_or("missing stdin")?;
    let output = BufReader::new(child.stdout.take().ok_or("missing stdout")?);
    Ok((child, input, output))
}

async fn send(input: &mut ChildStdin, id: u64, method: &str, params: Value) -> TestResult {
    let mut bytes = serde_json::to_vec(&Message::new(Body::Request {
        id,
        method: method.into(),
        params,
    }))?;
    bytes.push(b'\n');
    input.write_all(&bytes).await?;
    input.flush().await?;
    Ok(())
}

async fn receive(output: &mut BufReader<ChildStdout>) -> Result<Message, TestError> {
    let mut line = String::new();
    if tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line)).await?? == 0 {
        return Err("unexpected provider EOF".into());
    }
    if line.len() > moly_protocol::MAX_FRAME_BYTES + 1 {
        return Err("oversized frame".into());
    }
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test]
async fn binary_handshake_validate_unknown_method_and_clean_eof() -> TestResult {
    let (mut child, mut input, mut output) = spawn()?;
    send(&mut input, 1, "initialize", json!({"protocol_version": 1})).await?;
    assert!(
        matches!(receive(&mut output).await?.body, Body::Response { id: 1, result } if result == json!({"role": "model_provider", "protocol_version": 1}))
    );
    send(
        &mut input,
        2,
        "provider.validate",
        json!({"model_endpoint": "https://example.invalid/exact", "model": "mock"}),
    )
    .await?;
    assert!(matches!(
        receive(&mut output).await?.body,
        Body::Response {
            id: 2,
            result: Value::Null
        }
    ));
    send(
        &mut input,
        3,
        "private-unknown-method",
        json!({"private-argument": true}),
    )
    .await?;
    let response = receive(&mut output).await?;
    assert!(!serde_json::to_string(&response)?.contains("private"));
    assert!(
        matches!(response.body, Body::Error { id: Some(3), error } if error.code == "unknown_method")
    );
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await??
            .success()
    );
    let mut remaining = Vec::new();
    output.read_to_end(&mut remaining).await?;
    assert!(remaining.is_empty());
    Ok(())
}

#[tokio::test]
async fn binary_exits_promptly_on_stdin_eof_while_http_is_running() -> TestResult {
    // Check both waiting for headers and waiting for a response body.
    for response_headers in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/private-endpoint", listener.local_addr()?);
        let (mut child, mut input, mut output) = spawn()?;
        send(&mut input, 1, "initialize", json!({"protocol_version": 1})).await?;
        receive(&mut output).await?;
        send(
            &mut input,
            2,
            "provider.step",
            json!({
                "options": {"model_endpoint": endpoint, "model": "mock"},
                "credential": "private-credential",
                "context": {
                    "session_id": "00000000-0000-4000-8000-000000000001",
                    "run_id": "00000000-0000-4000-8000-000000000002",
                    "model_call_id": "00000000-0000-4000-8000-000000000003",
                    "call_kind": "primary"
                },
                "messages": [{"kind": "user", "text": "private-prompt"}],
                "tools": []
            }),
        )
        .await?;
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await??;
        let mut http = BufReader::new(stream);
        let mut length = None;
        loop {
            let mut line = String::new();
            let count =
                tokio::time::timeout(Duration::from_secs(5), http.read_line(&mut line)).await??;
            if count == 0 {
                return Err("unexpected HTTP EOF".into());
            }
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>()?);
            }
        }
        let mut body = vec![0; length.ok_or("missing content length")?];
        http.read_exact(&mut body).await?;
        if response_headers {
            http.get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                .await?;
        }
        drop(input);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await??
                .success()
        );
        let mut remaining = Vec::new();
        output.read_to_end(&mut remaining).await?;
        assert!(remaining.is_empty());
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .ok_or("missing stderr")?
            .read_to_string(&mut stderr)
            .await?;
        assert!(!stderr.contains("private"));
        let mut byte = [0];
        match tokio::time::timeout(Duration::from_secs(1), http.read(&mut byte)).await? {
            Ok(0) => {}
            // Dropping a socket with unread response bytes may send TCP RST.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            _ => return Err("HTTP request was not disconnected".into()),
        }
    }
    Ok(())
}

#[tokio::test]
async fn malformed_frame_exits_without_waiting_for_stdin_eof() -> TestResult {
    let (mut child, mut input, mut output) = spawn()?;
    input.write_all(b"private-malformed-json\n").await?;
    let reply = receive(&mut output).await?;
    assert!(!serde_json::to_string(&reply)?.contains("private"));
    assert!(matches!(reply.body, Body::Error { id: None, error } if error.code == "invalid_frame"));
    // Keep stdin open: a blocking runtime stdin worker must not delay shutdown.
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await??
            .success()
    );
    drop(input);
    Ok(())
}
