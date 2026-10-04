//! Process-boundary smoke tests; no credentials or live service access.

use std::process::Stdio;
use std::time::Duration;

use moly_protocol::{Body, Message};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn spawn() -> Result<Child, std::io::Error> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_moly-provider-openai-codex"));
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Needed for process loading on Windows, not credential/config discovery.
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command.spawn()
}

async fn request(stdin: &mut ChildStdin, id: u64, method: &str, params: Value) -> TestResult {
    let message = Message::new(Body::Request {
        id,
        method: method.into(),
        params,
    });
    let mut bytes = serde_json::to_vec(&message)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    Ok(())
}

async fn receive(
    stdout: &mut BufReader<ChildStdout>,
) -> Result<Message, Box<dyn std::error::Error + Send + Sync>> {
    let mut line = String::new();
    let count = tokio::time::timeout(Duration::from_secs(3), stdout.read_line(&mut line)).await??;
    if count == 0 {
        return Err("unexpected provider EOF".into());
    }
    assert!(line.ends_with('\n'));
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test]
async fn binary_is_v2_network_free_validation_and_eof_exits() -> TestResult {
    let mut child = spawn()?;
    let mut stdin = child.stdin.take().ok_or("stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("stdout")?);
    request(&mut stdin, 1, "initialize", json!({"protocol_version":2})).await?;
    assert!(
        matches!(receive(&mut stdout).await?.body,Body::Response { id:1,result } if result == json!({"role":"model_provider","protocol_version":2}))
    );
    request(
        &mut stdin,
        2,
        "provider.validate",
        json!({"model":"explicit-model","host_id":"urn:uuid:00000000-0000-4000-8000-000000000001"}),
    )
    .await?;
    assert!(matches!(
        receive(&mut stdout).await?.body,
        Body::Response {
            id: 2,
            result: Value::Null
        }
    ));
    request(
        &mut stdin,
        3,
        "future.method",
        json!({"credential":"synthetic-private-sentinel"}),
    )
    .await?;
    let error = receive(&mut stdout).await?;
    assert!(!serde_json::to_string(&error)?.contains("synthetic-private-sentinel"));
    assert!(
        matches!(error.body,Body::Error { id:Some(3),error } if error.code == "unknown_method")
    );
    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await??
            .success()
    );
    assert!(receive(&mut stdout).await.is_err());
    Ok(())
}

#[tokio::test]
async fn malformed_envelope_is_sanitized_and_process_fails_closed() -> TestResult {
    let mut child = spawn()?;
    let mut stdin = child.stdin.take().ok_or("stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("stdout")?);
    stdin
        .write_all(b"{\"synthetic-private-sentinel\":true}\n")
        .await?;
    let response = receive(&mut stdout).await?;
    assert!(matches!(response.body,Body::Error { id:None,error } if error.code == "invalid_frame"));
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output()).await??;
    assert!(!output.status.success());
    assert!(!String::from_utf8(output.stderr)?.contains("synthetic-private-sentinel"));
    Ok(())
}
