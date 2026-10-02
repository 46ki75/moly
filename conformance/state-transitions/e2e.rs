//! Hermetic end-to-end checks through a Server process, local IPC, and gated HTTP inference.

use crate::{
    client::{Client, Events, Tool},
    tests::server_process::ServerProcess,
    transport::{self, Incoming, Peer},
};
use moly_protocol::{
    ConfigSnapshot, EventKind, ExecutorId, Initialized, ResolvedConfig, RunId,
    SERVER_VERSION as VERSION, ServerId, SessionEvent, SessionId, ToolDefinition, ToolExecute,
    ToolResult, ToolsRegister,
};
use serde_json::{Value, json};
#[path = "../support/provider_process.rs"]
mod provider_process;
use std::{error::Error, future::Future, io, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

type TestError = Box<dyn Error + Send + Sync>;
type TestResult = Result<(), TestError>;

async fn bounded(future: impl Future<Output = TestResult>) -> TestResult {
    tokio::time::timeout(Duration::from_secs(15), future).await?
}

// JoinHandle normally detaches on drop, including when an outer test times out.
struct Task<T>(Option<JoinHandle<T>>);
impl<T> Task<T> {
    async fn join(&mut self) -> Result<T, TestError> {
        let result = self.0.as_mut().ok_or("task already joined")?.await?;
        self.0.take();
        Ok(result)
    }
}
impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

struct ProviderRequest {
    headers: String,
    body: Value,
    reply: oneshot::Sender<Value>,
    sent: oneshot::Receiver<io::Result<()>>,
}
impl ProviderRequest {
    async fn release(self, response: Value) -> Result<io::Result<()>, TestError> {
        self.reply
            .send(response)
            .map_err(|_| "mock response receiver closed")?;
        Ok(self.sent.await?)
    }

    async fn respond(self, response: Value) -> TestResult {
        self.release(response).await??;
        Ok(())
    }
}

async fn capture(stream: &mut TcpStream) -> Result<(String, Value), TestError> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 || bytes.len() + count > 1024 * 1024 {
            return Err("incomplete or oversized mock HTTP headers".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec())?;
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .ok_or("mock HTTP request lacks content-length")?
        .1
        .trim()
        .parse()?;
    if length > 1024 * 1024 - end {
        return Err("oversized mock HTTP body".into());
    }
    while bytes.len() < end + length {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Err("incomplete mock HTTP body".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok((headers, serde_json::from_slice(&bytes[end..end + length])?))
}

async fn handle_http(mut stream: TcpStream, requests: mpsc::Sender<ProviderRequest>) -> TestResult {
    let (headers, body) = capture(&mut stream).await?;
    let (reply, response) = oneshot::channel();
    let (sent, written) = oneshot::channel();
    requests
        .send(ProviderRequest {
            headers,
            body,
            reply,
            sent: written,
        })
        .await?;
    let body = serde_json::to_vec(&response.await?)?;
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let result = async {
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.shutdown().await
    }
    .await;
    // A cancelled inference may have already closed its HTTP connection.
    let _ = sent.send(result);
    Ok(())
}

struct Fixture {
    server: ServerProcess,
    directory: TempDir,
    endpoint: String,
    model_endpoint: String,
    shutdown: CancellationToken,
    http: Task<TestResult>,
    requests: mpsc::Receiver<ProviderRequest>,
}
impl Fixture {
    async fn new() -> Result<Self, TestError> {
        let directory = tempfile::Builder::new().prefix("moly-").tempdir()?;
        #[cfg(unix)]
        let endpoint = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
            directory
                .path()
                .join("s")
                .to_str()
                .ok_or("non-UTF-8 IPC endpoint")?
                .to_owned()
        };
        #[cfg(windows)]
        let endpoint = format!("moly-e2e-{}", moly_protocol::ConnectionId::new());
        let tcp = TcpListener::bind("127.0.0.1:0").await?;
        let model_endpoint = format!("http://{}/v1/chat/completions", tcp.local_addr()?);
        let (requests_tx, requests) = mpsc::channel(16);
        let shutdown = CancellationToken::new();
        let http_shutdown = shutdown.clone();
        let http = Task(Some(tokio::spawn(async move {
            let mut workers = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = http_shutdown.cancelled() => return Ok(()),
                    Some(result) = workers.join_next(), if !workers.is_empty() => { result??; },
                    stream = tcp.accept() => {
                        let (stream, _) = stream?;
                        workers.spawn(handle_http(stream, requests_tx.clone()));
                    }
                }
            }
        })));
        let server = ServerProcess::spawn(&endpoint).await?;
        Ok(Self {
            server,
            directory,
            endpoint,
            model_endpoint,
            shutdown,
            http,
            requests,
        })
    }

    async fn client(&self) -> Result<(Client, Events), TestError> {
        let (client, events) = Client::connect(&self.endpoint).await?;
        assert_eq!(client.server_id(), self.server.server_id);
        Ok((client, events))
    }

    fn config(&self) -> Result<ResolvedConfig, TestError> {
        Ok(ResolvedConfig {
            provider: provider_process::provider(
                json!({"model_endpoint":self.model_endpoint, "model":"test-model"}),
            )?,
            workspace: self
                .directory
                .path()
                .to_str()
                .ok_or("non-UTF-8 workspace")?
                .to_owned(),
            secret_ref: None,
        })
    }

    async fn configure(&self, client: &Client) -> TestResult {
        assert_eq!(client.apply_config(0, self.config()?).await?.revision, 1);
        Ok(())
    }

    async fn next_request(&mut self) -> Result<ProviderRequest, TestError> {
        let request = self.requests.recv().await.ok_or("mock provider stopped")?;
        assert!(
            request
                .headers
                .starts_with("POST /v1/chat/completions HTTP/1.1\r\n")
        );
        assert!(
            !request
                .headers
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        assert_eq!(request.body["stream"], false);
        assert_eq!(request.body["n"], 1);
        assert_eq!(request.body["model"], "test-model");
        Ok(request)
    }

    async fn raw(&self) -> Result<(Peer, transport::Inbox), TestError> {
        Ok(Peer::spawn(
            transport::local::connect(&self.endpoint).await?,
        ))
    }

    async fn stop(&mut self) -> TestResult {
        self.shutdown.cancel();
        self.server.stop().await?;
        self.http.join().await??;
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn final_response(text: &str) -> Value {
    json!({"object":"chat.completion", "choices":[{
        "index":0, "finish_reason":"stop", "message":{"role":"assistant", "content":text}
    }]})
}

fn tool_response(name: &str, arguments: Value) -> Value {
    json!({"object":"chat.completion", "choices":[{
        "index":0, "finish_reason":"tool_calls", "message":{
            "role":"assistant", "content":null, "tool_calls":[{
                "id":"call-1", "type":"function",
                "function":{"name":name, "arguments":arguments.to_string()}
            }]
        }
    }]})
}

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "client_echo".into(),
        description: "Echo through a Client-hosted tool".into(),
        input_schema: json!({"type":"object", "properties":{"text":{"type":"string"}}, "required":["text"]}),
    }
}

async fn read_events(events: &mut Events, count: usize) -> Result<Vec<SessionEvent>, TestError> {
    let mut result = Vec::new();
    for _ in 0..count {
        result.push(events.recv().await.ok_or("event connection closed")?);
    }
    Ok(result)
}

async fn until_terminal(events: &mut Events, run: RunId) -> Result<Vec<SessionEvent>, TestError> {
    let mut result = Vec::new();
    loop {
        let event = events.recv().await.ok_or("event connection closed")?;
        let terminal = event.kind.terminal_run() == Some(run);
        result.push(event);
        if terminal {
            return Ok(result);
        }
        if result.len() > 64 {
            return Err("unexpectedly many events before terminal transition".into());
        }
    }
}

fn contiguous(events: &[SessionEvent], session: SessionId, first: u64) {
    let selected: Vec<_> = events
        .iter()
        .filter(|event| event.session_id == session)
        .collect();
    assert!(!selected.is_empty());
    for (index, event) in selected.iter().enumerate() {
        assert_eq!(event.seq, first + index as u64);
    }
}

fn assert_remote<T>(result: Result<T, crate::client::Error>, code: &str) {
    assert!(matches!(result, Err(crate::client::Error::Remote(error)) if error.code == code));
}

fn assert_wire_remote(result: Result<Value, transport::Error>, code: &str) {
    assert!(matches!(result, Err(transport::Error::Remote(error)) if error.code == code));
}

#[tokio::test]
async fn two_clients_share_config_cas_and_replay_a_local_tool_run() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (first, mut events) = fixture.client().await?;
        let (second, mut replay) = fixture.client().await?;
        assert_eq!(first.server_id(), fixture.server.server_id);
        assert_eq!(second.server_id(), first.server_id());
        assert_eq!(first.config().await?.revision, 0);
        assert!(second.config().await?.config.is_none());
        fixture.configure(&first).await?;
        assert_eq!(second.config().await?.config, Some(fixture.config()?));
        let mut changed = fixture.config()?;
        changed.provider.options["model"] = json!("changed-model");
        assert_remote(second.apply_config(0, changed.clone()).await, "revision_conflict");
        assert_eq!(first.config().await?.revision, 1);

        tokio::fs::write(fixture.directory.path().join("notes.txt"), "hermetic file\n").await?;
        let session = first.create_session().await?;
        assert_eq!(first.subscribe(session, 0).await?, 1);
        let run = first.start_run(session, "Read notes.txt".into()).await?;
        let request = fixture.next_request().await?;
        assert_remote(second.start_run(session, "must not overlap".into()).await, "run_busy");
        assert_remote(second.register_tools(session, vec![]).await, "run_busy");
        assert_eq!(request.body["messages"], json!([{"role":"user", "content":"Read notes.txt"}]));
        assert_eq!(request.body["tools"][0]["function"]["name"], "read_file");
        // The active run must keep its accepted snapshot across a global update.
        assert_eq!(second.apply_config(1, changed).await?.revision, 2);
        request.respond(tool_response("read_file", json!({"path":"notes.txt"}))).await?;
        let request = fixture.next_request().await?;
        assert_eq!(request.body["messages"][1]["role"], "assistant");
        let tool_message = &request.body["messages"][2];
        assert_eq!(tool_message["role"], "tool");
        assert_eq!(tool_message["tool_call_id"], "call-1");
        assert_eq!(serde_json::from_str::<Value>(tool_message["content"].as_str().ok_or("missing tool content")?)?, json!({"content":"hermetic file\n"}));
        request.respond(final_response("File read successfully")).await?;
        let committed = until_terminal(&mut events, run).await?;
        contiguous(&committed, session, 1);
        assert_eq!(committed.len(), 9);
        assert!(matches!(committed[0].kind, EventKind::SessionCreated));
        assert!(matches!(committed[1].kind, EventKind::MessageAccepted { .. }));
        assert!(matches!(committed[2].kind, EventKind::RunStarted { run_id, config_revision: 1 } if run_id == run));
        let (first_call, second_call) = match (&committed[3].kind, &committed[6].kind) {
            (EventKind::ModelCallStarted { model_call_id: first, .. }, EventKind::ModelCallStarted { model_call_id: second, .. }) => (first, second),
            _ => return Err("missing model transitions".into()),
        };
        assert_ne!(first_call, second_call);
        match (&committed[4].kind, &committed[5].kind) {
            (EventKind::ToolStarted { lease, name, .. }, EventKind::ToolCompleted { lease: completed, .. }) => {
                assert_eq!(name, "read_file");
                assert_eq!(lease, completed);
                assert_eq!(lease.generation, 1);
            }
            _ => return Err("missing tool transitions".into()),
        }
        assert!(matches!(&committed[7].kind, EventKind::AssistantMessage { run_id, text } if *run_id == run && text == "File read successfully"));
        assert!(matches!(committed[8].kind, EventKind::RunCompleted { run_id } if run_id == run));
        assert_eq!(second.subscribe(session, 4).await?, 9);
        let suffix = read_events(&mut replay, 5).await?;
        contiguous(&suffix, session, 5);
        assert_eq!(serde_json::to_value(&suffix)?, serde_json::to_value(&committed[4..])?);
        assert_remote(second.subscribe(session, 10).await, "invalid_seq");
        first.close();
        second.close();
        fixture.stop().await
    }).await
}

#[tokio::test]
async fn disconnect_during_gated_model_call_does_not_cancel_the_run() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (client, mut events) = fixture.client().await?;
        fixture.configure(&client).await?;
        let server_id = client.server_id();
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let run = client.start_run(session, "Finish after disconnect".into()).await?;
        let pending = fixture.next_request().await?;
        let before = read_events(&mut events, 4).await?;
        contiguous(&before, session, 1);
        client.close();
        assert!(events.recv().await.is_none());
        drop(client);
        pending.respond(final_response("Still completed")).await?;
        let (resumed, mut replay) = fixture.client().await?;
        assert_eq!(resumed.server_id(), server_id);
        let head = resumed.subscribe(session, 4).await?;
        assert!((4..=6).contains(&head));
        let suffix = until_terminal(&mut replay, run).await?;
        contiguous(&suffix, session, 5);
        assert_eq!(suffix.len(), 2);
        assert!(matches!(&suffix[0].kind, EventKind::AssistantMessage { text, .. } if text == "Still completed"));
        assert!(matches!(suffix[1].kind, EventKind::RunCompleted { run_id } if run_id == run));
        resumed.close();
        fixture.stop().await
    }).await
}

#[tokio::test]
async fn cancellation_fences_a_late_model_result_from_the_replacement_run() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (client, mut events) = fixture.client().await?;
        fixture.configure(&client).await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let cancelled = client.start_run(session, "Cancel this".into()).await?;
        let late = fixture.next_request().await?;
        client.cancel_run(session, cancelled).await?;
        let mut committed = until_terminal(&mut events, cancelled).await?;
        assert_eq!(committed.len(), 5);
        assert!(matches!(committed[4].kind, EventKind::RunCancelled { run_id } if run_id == cancelled));
        let replacement = client.start_run(session, "Replace it".into()).await?;
        assert_ne!(cancelled, replacement);
        let fresh = fixture.next_request().await?;
        assert_eq!(fresh.body["messages"], json!([
            {"role":"user", "content":"Cancel this"}, {"role":"user", "content":"Replace it"}
        ]));
        // Await the attempted late write, even if cancellation closed its socket.
        let _ = late.release(final_response("obsolete result must never commit")).await?;
        fresh.respond(final_response("fresh result")).await?;
        committed.extend(until_terminal(&mut events, replacement).await?);
        contiguous(&committed, session, 1);
        assert_eq!(committed.len(), 10);
        assert_eq!(committed.iter().filter(|event| event.kind.terminal_run() == Some(cancelled)).count(), 1);
        assert!(matches!(&committed[8].kind, EventKind::AssistantMessage { run_id, text } if *run_id == replacement && text == "fresh result"));
        assert!(matches!(committed[9].kind, EventKind::RunCompleted { run_id } if run_id == replacement));
        let (observer, mut replay) = fixture.client().await?;
        assert_eq!(observer.subscribe(session, 4).await?, 10);
        assert_eq!(serde_json::to_value(read_events(&mut replay, 6).await?)?, serde_json::to_value(&committed[4..])?);
        assert_remote(client.cancel_run(session, cancelled).await, "not_active");
        observer.close();
        client.close();
        fixture.stop().await
    }).await
}

#[tokio::test]
async fn a_gated_session_does_not_block_another_session() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (client, mut events) = fixture.client().await?;
        fixture.configure(&client).await?;
        let slow_session = client.create_session().await?;
        let fast_session = client.create_session().await?;
        client.subscribe(slow_session, 0).await?;
        client.subscribe(fast_session, 0).await?;
        let slow_run = client.start_run(slow_session, "slow".into()).await?;
        let slow = fixture.next_request().await?;
        let fast_run = client.start_run(fast_session, "fast".into()).await?;
        let fast = fixture.next_request().await?;
        assert_eq!(fast.body["messages"][0]["content"], "fast");
        fast.respond(final_response("fast finished first")).await?;
        let mut committed = until_terminal(&mut events, fast_run).await?;
        assert!(matches!(committed.last().ok_or("missing fast terminal")?.kind, EventKind::RunCompleted { run_id } if run_id == fast_run));
        assert!(!committed.iter().any(|event| event.kind.terminal_run() == Some(slow_run)));
        assert_eq!(client.config().await?.revision, 1);
        slow.respond(final_response("slow finished second")).await?;
        committed.extend(until_terminal(&mut events, slow_run).await?);
        contiguous(&committed, slow_session, 1);
        contiguous(&committed, fast_session, 1);
        assert_eq!(committed.iter().filter(|event| event.session_id == slow_session).count(), 6);
        assert_eq!(committed.iter().filter(|event| event.session_id == fast_session).count(), 6);
        client.close();
        fixture.stop().await
    }).await
}

#[tokio::test]
async fn hosted_tool_can_make_a_nested_ordinary_request_on_the_same_client() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (client, mut events) = fixture.client().await?;
        let client = Arc::new(client);
        fixture.configure(&client).await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        // A weak capture avoids a Client -> handler -> Client ownership cycle.
        let weak = Arc::downgrade(&client);
        let (executed, mut observed) = mpsc::channel(1);
        let tool = Tool::new(tool_definition(), move |arguments| {
            let weak = weak.clone();
            let executed = executed.clone();
            async move {
                let client = weak.upgrade().ok_or_else(|| {
                    moly_protocol::ProtocolError::new("test_closed", "test Client dropped")
                })?;
                let snapshot = client.config().await.map_err(|_| {
                    moly_protocol::ProtocolError::new("test_nested", "nested request failed")
                })?;
                let output = json!({"revision":snapshot.revision, "echo":arguments});
                executed.send(output.clone()).await.map_err(|_| {
                    moly_protocol::ProtocolError::new("test_closed", "test observer dropped")
                })?;
                Ok(output)
            }
        });
        let executor = client.register_tools(session, vec![tool]).await?;
        let run = client.start_run(session, "Use client_echo".into()).await?;
        let request = fixture.next_request().await?;
        assert_eq!(request.body["tools"][1]["function"]["name"], "client_echo");
        request
            .respond(tool_response("client_echo", json!({"text":"duplex"})))
            .await?;
        let output = observed
            .recv()
            .await
            .ok_or("tool callback did not execute")?;
        assert_eq!(output, json!({"revision":1, "echo":{"text":"duplex"}}));
        let request = fixture.next_request().await?;
        assert_eq!(
            serde_json::from_str::<Value>(
                request.body["messages"][2]["content"]
                    .as_str()
                    .ok_or("missing remote tool output")?
            )?,
            output
        );
        request.respond(final_response("duplex completed")).await?;
        let committed = until_terminal(&mut events, run).await?;
        contiguous(&committed, session, 1);
        assert_eq!(committed.len(), 9);
        match (&committed[4].kind, &committed[5].kind) {
            (
                EventKind::ToolStarted { lease, .. },
                EventKind::ToolCompleted {
                    lease: completed, ..
                },
            ) => {
                assert_eq!(lease.executor_id, executor);
                assert_eq!(lease, completed);
            }
            _ => return Err("remote tool did not commit a matching result".into()),
        }
        assert!(matches!(committed[8].kind, EventKind::RunCompleted { run_id } if run_id == run));
        client.close();
        fixture.stop().await
    })
    .await
}

#[tokio::test]
async fn losing_the_assigned_executor_fails_work_but_preserves_the_session() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (client, mut events) = fixture.client().await?;
        fixture.configure(&client).await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let (executor, mut inbox) = fixture.raw().await?;
        executor.request("initialize", json!({"protocol_version":VERSION})).await?;
        executor.request("tools.register", serde_json::to_value(ToolsRegister { session_id:session, tools:vec![tool_definition()] })?).await?;
        let lost = client.start_run(session, "Use the disconnecting executor".into()).await?;
        fixture.next_request().await?.respond(tool_response("client_echo", json!({"text":"lost"}))).await?;
        assert!(matches!(inbox.recv().await, Some(Incoming::Request { method, .. }) if method == "tool.execute"));
        executor.close();
        let failed = until_terminal(&mut events, lost).await?;
        contiguous(&failed, session, 1);
        assert!(matches!(&failed.last().ok_or("missing terminal event")?.kind, EventKind::RunFailed { error, .. } if error.code == "executor_lost"));
        let next = client.start_run(session, "The session still exists".into()).await?;
        fixture.next_request().await?.respond(final_response("still alive")).await?;
        let recovered = until_terminal(&mut events, next).await?;
        contiguous(&recovered, session, failed.len() as u64 + 1);
        assert!(matches!(recovered.last().ok_or("missing terminal event")?.kind, EventKind::RunCompleted { run_id } if run_id == next));
        client.close();
        fixture.stop().await
    }).await
}

#[tokio::test]
async fn a_raw_executor_cannot_commit_a_mismatched_lease() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (client, mut events) = fixture.client().await?;
        fixture.configure(&client).await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let (raw, mut inbox) = fixture.raw().await?;
        raw.request("initialize", json!({"protocol_version":VERSION})).await?;
        let registered = raw.request("tools.register", serde_json::to_value(ToolsRegister { session_id: session, tools: vec![tool_definition()] })?).await?;
        let executor: ExecutorId = serde_json::from_value(registered["executor_id"].clone())?;
        let run = client.start_run(session, "Reject wrong authority".into()).await?;
        fixture.next_request().await?.respond(tool_response("client_echo", json!({"text":"wrong lease"}))).await?;
        let (id, request) = match inbox.recv().await.ok_or("raw executor disconnected")? {
            Incoming::Request { id, method, params } => {
                assert_eq!(method, "tool.execute");
                (id, serde_json::from_value::<ToolExecute>(params)?)
            }
            _ => return Err("expected reverse tool request".into()),
        };
        assert_eq!(request.session_id, session);
        assert_eq!(request.lease.executor_id, executor);
        assert_eq!(request.arguments, json!({"text":"wrong lease"}));
        let mut wrong = request.lease;
        wrong.generation += 1;
        raw.respond(id, Ok(serde_json::to_value(ToolResult { lease: wrong, output: json!("must not commit") })?)).await?;
        let committed = until_terminal(&mut events, run).await?;
        contiguous(&committed, session, 1);
        assert_eq!(committed.len(), 6);
        assert!(matches!(&committed[5].kind, EventKind::RunFailed { run_id, error } if *run_id == run && error.code == "stale_tool_result"));
        assert!(!committed.iter().any(|event| matches!(event.kind, EventKind::ToolCompleted { .. } | EventKind::AssistantMessage { .. })));
        raw.close();
        client.close();
        fixture.stop().await
    }).await
}

#[tokio::test]
async fn initialization_gates_commands_and_unknown_methods_are_correlated_errors() -> TestResult {
    bounded(async {
        let mut fixture = Fixture::new().await?;
        let (peer, _inbox) = fixture.raw().await?;
        assert_wire_remote(
            peer.request("config.get", Value::Null).await,
            "not_initialized",
        );
        assert_wire_remote(
            peer.request("unknown.method", Value::Null).await,
            "not_initialized",
        );
        assert_wire_remote(
            peer.request("initialize", json!({"protocol_version":VERSION + 1}))
                .await,
            "incompatible_version",
        );
        let initialized: Initialized = serde_json::from_value(
            peer.request("initialize", json!({"protocol_version":VERSION}))
                .await?,
        )?;
        let identity: ServerId = initialized.server_id;
        assert_eq!(identity, fixture.server.server_id);
        assert_eq!(initialized.role, "server");
        assert_eq!(initialized.protocol_version, VERSION);
        assert_wire_remote(
            peer.request("initialize", json!({"protocol_version":VERSION}))
                .await,
            "already_initialized",
        );
        assert_wire_remote(
            peer.request("unknown.method", Value::Null).await,
            "unknown_method",
        );
        let config: ConfigSnapshot =
            serde_json::from_value(peer.request("config.get", Value::Null).await?)?;
        assert_eq!(config.revision, 0);
        assert!(config.config.is_none());
        peer.close();
        fixture.stop().await
    })
    .await
}
