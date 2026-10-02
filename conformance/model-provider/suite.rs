//! Real-process, cross-language tests. No Provider runtime code is shared with
//! the Python peer, and all Client traffic uses the SDK's public facade.
#[path = "audited_server.rs"]
mod audited_server;

use crate::server_process::ServerProcess;
use audited_server::AuditedServer;
use moly_client::{
    Client, Error, Events, Tool,
    protocol::{
        EventKind, ResolvedConfig, RunId, SessionEvent, SessionId, ToolDefinition,
        model::{ComponentCommand, ProviderConfig},
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), TestError>;
const SELECTED: &str = "selected-credential-secret-sentinel";
const OTHER: &str = "unselected-credential-secret-sentinel";
const USER: &str = "provider-user-content-sentinel: α\nsecond line";

async fn bounded(future: impl Future<Output = TestResult>) -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), future).await?
}

struct Inputs {
    directory: TempDir,
    endpoint: String,
    python: String,
    #[cfg(target_os = "macos")]
    cf_encoding: Option<String>,
    fixture: String,
    audit: PathBuf,
    stderr: PathBuf,
}

impl Inputs {
    async fn new() -> Result<Self, TestError> {
        let directory = tempfile::Builder::new().prefix("moly-p-").tempdir()?;
        #[cfg(unix)]
        let endpoint = directory.path().join("s").to_string_lossy().into_owned();
        #[cfg(windows)]
        let endpoint = format!(
            "moly-provider-{}",
            moly_client::protocol::ConnectionId::new()
        );
        let executable = if cfg!(windows) { "python" } else { "python3" };
        // Resolving sys.executable is Client policy, not Provider execution. The
        // fixture itself is launched only by the Server via ComponentCommand.
        let output = Command::new(executable)
            .args([
                "-I",
                "-c",
                "import json, os, sys; print(json.dumps({'executable': sys.executable, 'cf_encoding': os.environ.get('__CF_USER_TEXT_ENCODING')}))",
            ])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| io::Error::other(format!(
                "Python 3 is required for Provider conformance; install {executable} on PATH and rerun: {error}"
            )))?;
        if !output.status.success() {
            return Err(format!(
                "Python 3 resolution failed; ensure `{executable} -I -c 'import sys; print(sys.executable)'` works before rerunning Provider tests"
            ).into());
        }
        let resolution: Value = serde_json::from_slice(&output.stdout)?;
        let python = resolution["executable"]
            .as_str()
            .ok_or("Python resolution did not return sys.executable")?
            .to_owned();
        #[cfg(target_os = "macos")]
        let cf_encoding = resolution["cf_encoding"].as_str().map(str::to_owned);
        if !Path::new(&python).is_absolute() || !Path::new(&python).is_file() {
            return Err(
                "Python sys.executable must resolve to an existing absolute executable".into(),
            );
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance/model-provider/provider.py")
            .canonicalize()?
            .to_str()
            .ok_or("non-UTF-8 fixture path")?
            .to_owned();
        let audit = directory.path().join("provider-audit.jsonl");
        let stderr = directory.path().join("server-stderr.txt");
        Ok(Self {
            directory,
            endpoint,
            python,
            #[cfg(target_os = "macos")]
            cf_encoding,
            fixture,
            audit,
            stderr,
        })
    }

    fn config(&self, scenario: &str) -> ResolvedConfig {
        let mut environment = BTreeMap::from([
            ("EXPLICIT_COMPONENT_MARKER".into(), "explicit-only".into()),
            ("PYTHONUTF8".into(), "1".into()),
        ]);
        #[cfg(windows)]
        if let Ok(root) = env::var("SystemRoot") {
            // Python normalizes environment keys to uppercase on Windows.
            environment.insert("SYSTEMROOT".into(), root);
        }
        // Observed macOS Python startup inserts (and may overwrite) this entry
        // even with env_clear. Supply its measured value, preserving an exact
        // audit of the entire environment rather than ignoring unknown entries.
        #[cfg(target_os = "macos")]
        if let Some(encoding) = &self.cf_encoding {
            environment.insert("__CF_USER_TEXT_ENCODING".into(), encoding.clone());
        }
        // Python may otherwise coerce LC_CTYPE on Unix; set it explicitly so the
        // environment audit can compare exact entries, not just a denylist.
        environment.insert("LC_CTYPE".into(), "C".into());
        environment.insert("PYTHONCOERCECLOCALE".into(), "0".into());
        ResolvedConfig {
            provider: ProviderConfig {
                command: ComponentCommand {
                    executable: self.python.clone(),
                    args: vec![
                        // No user/system site customization may run in the fixture.
                        "-S".into(),
                        self.fixture.clone(),
                        scenario.into(),
                        self.audit.to_string_lossy().into_owned(),
                    ],
                    env: environment,
                },
                // An array is intentional: the Server must not demand an HTTP
                // endpoint, model name, or even an object from an implementation.
                options: json!([null, {"opaque":[true, 7]}, "implementation-defined"]),
            },
            workspace: self.directory.path().to_string_lossy().into_owned(),
            secret_ref: None,
        }
    }

    fn records(&self) -> Result<Vec<Value>, TestError> {
        if !self.audit.exists() {
            return Ok(Vec::new());
        }
        fs::read_to_string(&self.audit)?
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect()
    }
}

fn requests<'a>(records: &'a [Value], method: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["request"]["method"] == method)
        .collect()
}

fn assert_redacted(text: &str) {
    for sentinel in [
        "provider-message-secret-sentinel",
        "provider-code-secret-sentinel",
        "provider-stderr-secret-sentinel",
        "ambient-secret-must-not-reach-provider",
        SELECTED,
        OTHER,
        "provider-user-content-sentinel",
    ] {
        assert!(
            !text.contains(sentinel),
            "sensitive sentinel escaped: {sentinel}"
        );
    }
}

fn assert_remote(error: Error, code: &str) {
    assert_redacted(&error.to_string());
    match error {
        Error::Remote(error) => assert_eq!(error.code, code),
        other => panic!("expected remote {code}, received {other}"),
    }
}

async fn through_terminal(
    events: &mut Events,
    session: SessionId,
    run: RunId,
) -> Result<Vec<SessionEvent>, TestError> {
    let mut seen: Vec<SessionEvent> = Vec::new();
    loop {
        let event = events.recv().await.ok_or("Server event stream ended")?;
        assert_eq!(event.session_id, session);
        if let Some(previous) = seen.last() {
            assert_eq!(event.seq, previous.seq + 1);
        }
        let terminal = event.kind.terminal_run();
        seen.push(event);
        if let Some(terminal) = terminal {
            assert_eq!(
                terminal, run,
                "obsolete run mutated the replacement event stream"
            );
            return Ok(seen);
        }
    }
}

fn assert_completed(events: &[SessionEvent], run: RunId, expected: &str) {
    assert!(matches!(events.last().map(|event| &event.kind),
        Some(EventKind::RunCompleted { run_id }) if *run_id == run));
    let text: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::AssistantMessage { run_id, text } if *run_id == run => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, vec![expected]);
}

fn assert_failed(events: &[SessionEvent], run: RunId, code: &str) {
    match &events.last().expect("at least one terminal event").kind {
        EventKind::RunFailed { run_id, error } => {
            assert_eq!(*run_id, run);
            assert_eq!(error.code, code);
            assert_redacted(&error.to_string());
        }
        other => panic!("expected RunFailed, received {other:?}"),
    }
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        EventKind::AssistantMessage { .. }
            | EventKind::ToolStarted { .. }
            | EventKind::ToolCompleted { .. }
    )));
}

fn echo_tool(effects: Arc<AtomicUsize>) -> Tool {
    Tool::new(
        ToolDefinition {
            name: "component_echo".into(),
            description: "Independent hosted capability".into(),
            input_schema: json!({"type":"object"}),
        },
        move |arguments| {
            effects.fetch_add(1, Ordering::SeqCst);
            async move { Ok(json!({"echo":arguments, "checked":true})) }
        },
    )
}

#[tokio::test]
async fn independent_provider_normalizes_context_and_is_launched_by_server() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        assert_eq!(client.server_id(), server.server_id);
        client.put_secret("selected-key", SELECTED).await?;
        client.put_secret("other-key-must-not-leak", OTHER).await?;
        let mut config = inputs.config("conversation");
        config.secret_ref = Some("selected-key".into());
        let snapshot = client.apply_config(0, config.clone()).await?;
        assert_eq!(snapshot.config, Some(config.clone()));
        assert_redacted(&serde_json::to_string(&snapshot)?);
        let session = client.create_session().await?;
        let effects = Arc::new(AtomicUsize::new(0));
        let executor = client.register_tools(session, vec![echo_tool(effects.clone())]).await?;
        client.subscribe(session, 0).await?;
        let first = client.start_run(session, USER.into()).await?;
        let mut seen = through_terminal(&mut events, session, first).await?;
        assert_completed(&seen, first, "tool roundtrip complete");
        let second = client.start_run(session, "follow up".into()).await?;
        let second_events = through_terminal(&mut events, session, second).await?;
        assert_completed(&second_events, second, "second turn complete");
        assert_eq!(second_events[0].seq, seen.last().expect("first run events").seq + 1);
        seen.extend(second_events);
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        let started: Vec<_> = seen.iter().filter_map(|event| match &event.kind {
            EventKind::ToolStarted { lease, name, .. } => Some((lease, name)),
            _ => None,
        }).collect();
        let finished: Vec<_> = seen.iter().filter_map(|event| match &event.kind {
            EventKind::ToolCompleted { lease, .. } => Some(lease),
            _ => None,
        }).collect();
        assert_eq!(started.len(), 1);
        assert_eq!(finished, vec![started[0].0]);
        assert_eq!(started[0].0.executor_id, executor);
        assert_eq!(started[0].1, "component_echo");

        let records = inputs.records()?;
        let validations = requests(&records, "provider.validate");
        let steps = requests(&records, "provider.step");
        assert_eq!(validations.len(), 1);
        assert_eq!(steps.len(), 3);
        assert_eq!(requests(&records, "initialize").len(), 4);
        assert_eq!(records.len(), 8, "one initialization and one operation per process");
        assert_eq!(validations[0]["request"]["params"], config.provider.options);
        for pair in records.as_chunks::<2>().0 {
            assert_eq!(pair[0]["request"]["method"], "initialize");
            assert_eq!(pair[0]["request"]["params"], json!({"protocol_version":1}));
            assert_eq!(pair[0]["pid"], pair[1]["pid"]);
            assert_eq!(pair[0]["ppid"], server.pid, "exact OS parent must be the Server");
            assert_eq!(pair[0]["environment"], serde_json::to_value(&config.provider.command.env)?);
        }
        for pair in records.as_chunks::<2>().0.windows(2) {
            assert_ne!(pair[0][0]["instance_id"], pair[1][0]["instance_id"], "invocations must not reuse an instance even if an OS recycles a PID");
        }
        let audit = serde_json::to_string(&records)?;
        assert!(!audit.contains(OTHER));
        assert!(!audit.contains("other-key-must-not-leak"));
        assert!(!serde_json::to_string(&validations)?.contains(SELECTED));
        let model_calls: Vec<_> = seen.iter().filter_map(|event| match event.kind {
            EventKind::ModelCallStarted { run_id, model_call_id } => Some((run_id, model_call_id)),
            _ => None,
        }).collect();
        assert_eq!(model_calls.len(), 3);
        for (index, step) in steps.iter().enumerate() {
            let request = &step["request"]["params"];
            assert_eq!(request["credential"], SELECTED);
            assert_eq!(request["options"], config.provider.options);
            assert_eq!(request["context"], json!({
                "session_id":session, "run_id":model_calls[index].0,
                "model_call_id":model_calls[index].1, "call_kind":"primary"
            }));
            assert!(request["tools"].as_array().ok_or("tools must be an array")?.iter()
                .any(|tool| tool["name"] == "component_echo" && tool["input_schema"] == json!({"type":"object"})));
        }
        assert_ne!(model_calls[0].1, model_calls[1].1);
        assert_ne!(model_calls[1].1, model_calls[2].1);
        let arguments = json!({"nested":{"values":[true, null, 7]}});
        let user = json!({"kind":"user", "text":USER});
        let assistant = json!({"kind":"assistant", "text":null, "tool_calls":[{
            "id":"provider-call-1", "name":"component_echo", "arguments":arguments
        }], "metadata":null});
        let result = json!({"kind":"tool_result", "call_id":"provider-call-1",
            "output":{"echo":arguments, "checked":true}});
        assert_eq!(steps[0]["request"]["params"]["messages"], json!([user]));
        assert_eq!(steps[1]["request"]["params"]["messages"], json!([user, assistant, result]));
        assert_eq!(steps[2]["request"]["params"]["messages"], json!([
            user, assistant, result,
            {"kind":"assistant", "text":"tool roundtrip complete", "tool_calls":[], "metadata":null},
            {"kind":"user", "text":"follow up"}
        ]));
        server.stop().await?;
        assert_redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    }).await
}

#[tokio::test]
async fn provider_validation_is_opaque_and_launch_failures_never_fall_back() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, _events) = Client::connect(&inputs.endpoint).await?;
        for options in [
            Value::Null,
            json!(true),
            json!(42),
            json!("opaque"),
            json!({"x":[null]}),
        ] {
            let mut config = inputs.config("complete");
            config.provider.options = options.clone();
            client.validate_config(&config).await?;
            let records = inputs.records()?;
            assert_eq!(
                records.last().ok_or("validation not delivered")?["request"]["params"],
                options
            );
            assert_eq!(client.config().await?.revision, 0);
        }
        let good = inputs.config("complete");
        client.apply_config(0, good.clone()).await?;
        let before_missing = inputs.records()?.len();
        let mut missing = good.clone();
        missing.provider.command.executable = inputs
            .directory
            .path()
            .join("missing-provider")
            .to_string_lossy()
            .into_owned();
        assert_remote(
            client
                .apply_config(1, missing)
                .await
                .expect_err("no fallback for missing executable"),
            "provider_unavailable",
        );
        assert_eq!(
            inputs.records()?.len(),
            before_missing,
            "must not launch a different Provider"
        );
        for (scenario, code) in [
            ("bad_role", "provider_protocol"),
            ("bad_version", "provider_protocol"),
            ("bad_envelope_version", "provider_protocol"),
            ("bad_handshake_array", "provider_protocol"),
            ("validate_non_null", "provider_protocol"),
            ("validate_unknown_error", "provider_error"),
            ("validate_known_error", "invalid_config"),
        ] {
            let before = inputs.records()?.len();
            assert_remote(
                client
                    .apply_config(1, inputs.config(scenario))
                    .await
                    .expect_err("invalid Provider must not be accepted"),
                code,
            );
            let records = inputs.records()?;
            let invocation = &records[before..];
            assert_eq!(
                requests(invocation, "initialize").len(),
                1,
                "no retry: {scenario}"
            );
            let expected_validations = usize::from(!scenario.starts_with("bad_"));
            assert_eq!(
                requests(invocation, "provider.validate").len(),
                expected_validations
            );
            assert!(requests(invocation, "provider.step").is_empty());
            let snapshot = client.config().await?;
            assert_eq!(snapshot.revision, 1, "failed validation must not commit");
            assert_eq!(snapshot.config, Some(good.clone()));
        }
        server.stop().await?;
        assert_redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn malformed_provider_steps_fail_once_with_redacted_errors_and_stderr() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        client.put_secret("selected-key", SELECTED).await?;
        let cases = [
            ("eof", "provider_unavailable"),
            ("malformed", "provider_protocol"),
            ("mismatched_id", "provider_protocol"),
            ("oversized", "provider_protocol"),
            ("unknown_type", "provider_protocol"),
            ("unknown_outcome", "provider_protocol"),
            ("metadata_array", "provider_protocol"),
            ("tool_call_arrays", "provider_protocol"),
            ("unknown_error", "provider_error"),
            ("known_error", "invalid_secret"),
        ];
        for (revision, (scenario, code)) in cases.into_iter().enumerate() {
            let before = inputs.records()?.len();
            let mut config = inputs.config(scenario);
            config.secret_ref = Some("selected-key".into());
            client.apply_config(revision as u64, config).await?;
            let session = client.create_session().await?;
            client.subscribe(session, 0).await?;
            let run = client.start_run(session, USER.into()).await?;
            let seen = through_terminal(&mut events, session, run).await?;
            assert_failed(&seen, run, code);
            assert_eq!(
                seen.iter()
                    .filter(|event| matches!(event.kind, EventKind::ModelCallStarted { .. }))
                    .count(),
                1
            );
            let records = inputs.records()?;
            let invocation = &records[before..];
            assert_eq!(
                requests(invocation, "initialize").len(),
                2,
                "no implicit process retry: {scenario}"
            );
            assert_eq!(requests(invocation, "provider.validate").len(), 1);
            assert_eq!(requests(invocation, "provider.step").len(), 1);
            assert_eq!(invocation.len(), 4);
            client.unsubscribe(session).await?;
        }
        server.stop().await?;
        assert_redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn untrusted_tool_batches_are_rejected_before_any_effect() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = ServerProcess::spawn(&inputs.endpoint).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        assert_eq!(client.server_id(), server.server_id);
        let effects = Arc::new(AtomicUsize::new(0));
        for (revision, scenario) in ["duplicate_ids", "unadvertised_tool"]
            .into_iter()
            .enumerate()
        {
            client
                .apply_config(revision as u64, inputs.config(scenario))
                .await?;
            let session = client.create_session().await?;
            client
                .register_tools(session, vec![echo_tool(effects.clone())])
                .await?;
            client.subscribe(session, 0).await?;
            let run = client
                .start_run(session, "do not execute an invalid batch".into())
                .await?;
            let seen = through_terminal(&mut events, session, run).await?;
            assert_failed(&seen, run, "provider_protocol");
            assert_eq!(effects.load(Ordering::SeqCst), 0);
            client.unsubscribe(session).await?;
        }
        assert_eq!(requests(&inputs.records()?, "provider.step").len(), 2);
        server.stop().await?;
        Ok(())
    })
    .await
}

async fn accept_gate(
    listener: &TcpListener,
    session: SessionId,
    run: RunId,
) -> Result<BufReader<TcpStream>, TestError> {
    let (stream, _) = listener.accept().await?;
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    let ready: Value = serde_json::from_str(&line)?;
    assert_eq!(ready["context"]["session_id"], json!(session));
    assert_eq!(ready["context"]["run_id"], json!(run));
    Ok(stream)
}

#[tokio::test]
async fn cancellation_kills_gated_provider_and_replacement_can_complete() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut server = ServerProcess::spawn(&inputs.endpoint).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        assert_eq!(client.server_id(), server.server_id);
        let mut config = inputs.config("gate");
        config
            .provider
            .command
            .args
            .push(listener.local_addr()?.port().to_string());
        client.apply_config(0, config).await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let run = client
            .start_run(session, "cancel after explicit readiness".into())
            .await?;
        let mut gate = accept_gate(&listener, session, run).await?;
        client.cancel_run(session, run).await?;
        let seen = through_terminal(&mut events, session, run).await?;
        assert!(matches!(seen.last().map(|event| &event.kind),
            Some(EventKind::RunCancelled { run_id }) if *run_id == run));
        assert_eq!(
            gate.read(&mut [0]).await?,
            0,
            "process-owned gate must close on cancellation"
        );
        assert!(server.is_running()?);
        let replacement = client.start_run(session, "replacement".into()).await?;
        let mut gate = accept_gate(&listener, session, replacement).await?;
        gate.get_mut().write_all(b"+").await?;
        let seen = through_terminal(&mut events, session, replacement).await?;
        assert_completed(&seen, replacement, "gate released");
        assert_eq!(
            gate.read(&mut [0]).await?,
            0,
            "completed invocation must also be stopped"
        );
        let records = inputs.records()?;
        let steps = requests(&records, "provider.step");
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[1]["request"]["params"]["messages"],
            json!([
                {"kind":"user", "text":"cancel after explicit readiness"},
                {"kind":"user", "text":"replacement"}
            ])
        );
        assert!(
            steps
                .iter()
                .all(|step| step["request"]["params"]["credential"].is_null())
        );
        server.stop().await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn client_disconnect_does_not_cancel_gated_provider_and_replay_is_canonical() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut server = ServerProcess::spawn(&inputs.endpoint).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        let mut config = inputs.config("gate");
        config
            .provider
            .command
            .args
            .push(listener.local_addr()?.port().to_string());
        client.apply_config(0, config).await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let run = client
            .start_run(session, "survive disconnect".into())
            .await?;
        let mut gate = accept_gate(&listener, session, run).await?;
        let mut prefix = Vec::new();
        loop {
            let event = events.recv().await.ok_or("missing model start")?;
            let started = matches!(event.kind, EventKind::ModelCallStarted { .. });
            prefix.push(event);
            if started {
                break;
            }
        }
        client.close();
        client.closed().await;
        assert!(events.recv().await.is_none());
        drop(client);
        let (observer, mut replay) = Client::connect(&inputs.endpoint).await?;
        assert_eq!(observer.server_id(), server.server_id);
        let head = observer.subscribe(session, 0).await?;
        assert_eq!(head, prefix.last().ok_or("missing observed prefix")?.seq);
        let mut replayed = Vec::new();
        for _ in &prefix {
            replayed.push(replay.recv().await.ok_or("missing replay prefix")?);
        }
        assert_eq!(
            serde_json::to_value(&replayed)?,
            serde_json::to_value(&prefix)?
        );
        // A positive challenge/response, not a timing-based "still alive" check.
        gate.get_mut().write_all(b"?").await?;
        let mut pong = [0];
        gate.read_exact(&mut pong).await?;
        assert_eq!(&pong, b"!");
        gate.get_mut().write_all(b"+").await?;
        let suffix = through_terminal(&mut replay, session, run).await?;
        assert_eq!(suffix[0].seq, head + 1);
        assert_completed(&suffix, run, "gate released");
        assert_eq!(gate.read(&mut [0]).await?, 0);
        assert_eq!(
            requests(&inputs.records()?, "provider.step").len(),
            1,
            "reconnect must not restart inference"
        );
        replayed.extend(suffix);
        assert_eq!(
            observer.subscribe(session, 0).await?,
            replayed.last().ok_or("missing terminal")?.seq
        );
        for expected in replayed {
            let actual = replay.recv().await.ok_or("missing completed replay")?;
            assert_eq!(
                serde_json::to_value(actual)?,
                serde_json::to_value(expected)?
            );
        }
        assert!(server.is_running()?);
        server.stop().await?;
        Ok(())
    })
    .await
}
