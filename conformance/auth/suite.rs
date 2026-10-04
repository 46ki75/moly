//! Auth conformance against the real Server, an independent Python Provider,
//! and the public SDK. Fake credentials never contact a live service.
#[path = "stream_client.rs"]
mod stream_client;

use crate::audited_server::AuditedServer;
use moly_client::{
    Client, Error, Events, Interaction,
    protocol::{
        AuthAttemptId, EventKind, ResolvedConfig, RunId, SessionEvent,
        auth::{AuthCommand, AuthOperation, InteractionOutcome},
        model::{ComponentCommand, ProviderConfig},
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use stream_client::StreamClient;
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{Notify, oneshot},
};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), TestError>;
const OLD: &str = r#"{"token":"auth-secret-original-sentinel","generation":0}"#;
const LOGIN: &str = r#"{"token":"auth-secret-login-sentinel","generation":1}"#;
const OTHER: &str = "auth-secret-unselected-sentinel";
const URL: &str = "https://auth.example.invalid/sign-in?state=auth-url-private-sentinel";

async fn bounded(future: impl Future<Output = TestResult>) -> TestResult {
    tokio::time::timeout(Duration::from_secs(25), future).await?
}

struct Inputs {
    directory: TempDir,
    endpoint: String,
    python: String,
    audit: PathBuf,
    stderr: PathBuf,
}

impl Inputs {
    async fn new() -> Result<Self, TestError> {
        let directory = tempfile::Builder::new().prefix("moly-a-").tempdir()?;
        #[cfg(unix)]
        let endpoint = directory.path().join("s").to_string_lossy().into_owned();
        #[cfg(windows)]
        let endpoint = format!("moly-auth-{}", AuthAttemptId::new());
        let interpreter = if cfg!(windows) { "python" } else { "python3" };
        let output = Command::new(interpreter)
            .args(["-I", "-c", "import sys; print(sys.executable)"])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| {
                std::io::Error::other(format!(
                    "Python 3 is required for auth conformance: {error}"
                ))
            })?;
        assert!(
            output.status.success(),
            "Python executable resolution failed"
        );
        let python = String::from_utf8(output.stdout)?.trim().to_owned();
        assert!(Path::new(&python).is_absolute() && Path::new(&python).is_file());
        let audit = directory.path().join("fake-provider-audit.jsonl");
        let stderr = directory.path().join("server-stderr.txt");
        Ok(Self {
            directory,
            endpoint,
            python,
            audit,
            stderr,
        })
    }

    fn config(
        &self,
        scenario: &str,
        key: Option<&str>,
        gate: Option<u16>,
    ) -> Result<ResolvedConfig, TestError> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance/auth/provider.py")
            .canonicalize()?;
        let mut environment = BTreeMap::from([("PYTHONUTF8".into(), "1".into())]);
        #[cfg(windows)]
        if let Ok(root) = env::var("SystemRoot") {
            environment.insert("SYSTEMROOT".into(), root);
        }
        environment.insert("PYTHONCOERCECLOCALE".into(), "0".into());
        let mut options = json!({"scenario":scenario});
        if let Some(port) = gate {
            options["gate_port"] = json!(port);
        }
        Ok(ResolvedConfig {
            provider: ProviderConfig {
                command: ComponentCommand {
                    executable: self.python.clone(),
                    args: vec![
                        "-S".into(),
                        fixture.to_string_lossy().into_owned(),
                        self.audit.to_string_lossy().into_owned(),
                    ],
                    env: environment,
                },
                options,
            },
            workspace: self.directory.path().to_string_lossy().into_owned(),
            secret_ref: key.map(str::to_owned),
        })
    }

    fn records(&self) -> Result<Vec<Value>, TestError> {
        if !self.audit.exists() {
            return Ok(vec![]);
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
        .filter(|record| record["message"]["method"] == method)
        .collect()
}

fn redacted(text: &str) {
    for sentinel in ["auth-secret-", "auth-url-private-sentinel", URL] {
        assert!(!text.contains(sentinel), "sensitive auth data escaped");
    }
}

fn remote(error: Error, code: &str) {
    redacted(&error.to_string());
    match error {
        Error::Remote(error) => assert_eq!(error.code, code),
        other => panic!("expected remote {code}, received {other}"),
    }
}

fn command(operation: AuthOperation, revision: u64) -> AuthCommand {
    AuthCommand {
        attempt_id: AuthAttemptId::new(),
        operation,
        config_revision: revision,
    }
}

fn opened() -> Interaction {
    Interaction::new(|_| async { Ok(InteractionOutcome::Opened) })
}

async fn terminal(events: &mut Events, run: RunId) -> Result<Vec<SessionEvent>, TestError> {
    let mut seen: Vec<SessionEvent> = Vec::new();
    loop {
        let event = events.recv().await.ok_or("missing terminal event")?;
        if let Some(previous) = seen.last() {
            assert_eq!(event.seq, previous.seq + 1);
        }
        let end = event.kind.terminal_run();
        seen.push(event);
        if let Some(end) = end {
            assert_eq!(end, run);
            return Ok(seen);
        }
    }
}

fn completed(events: &[SessionEvent]) {
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(EventKind::RunCompleted { .. })
    ));
    redacted(&serde_json::to_string(events).expect("serializable events"));
}

async fn gate(listener: &TcpListener) -> Result<(BufReader<TcpStream>, Value), TestError> {
    let (stream, _) = listener.accept().await?;
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    assert_ne!(stream.read_line(&mut line).await?, 0);
    Ok((stream, serde_json::from_str(&line)?))
}

async fn child_stopped(stream: &mut BufReader<TcpStream>) -> TestResult {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), stream.read(&mut [0])).await??,
        0,
        "Provider-owned gate must close when the operation ends"
    );
    Ok(())
}

#[tokio::test]
async fn login_routes_only_to_initiator_and_replacement_is_private_and_fresh() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        assert_eq!(client.server_id(), server.server_id);
        let mut observer = StreamClient::connect(&inputs.endpoint).await?;
        client.put_secret("other-key", OTHER).await?;
        client
            .apply_config(
                0,
                inputs.config("sequential_login", Some("selected-key"), None)?,
            )
            .await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let auth = command(AuthOperation::Login, 1);
        let attempt = auth.attempt_id;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let nested = client.clone();
        let status = client
            .authenticate(
                auth,
                Some(Interaction::new(move |request| {
                    assert_eq!(request.attempt_id, attempt);
                    assert_eq!(request.url, URL);
                    count.fetch_add(1, Ordering::SeqCst);
                    let nested = nested.clone();
                    async move {
                        assert_eq!(
                            nested
                                .config()
                                .await
                                .map_err(|_| moly_client::protocol::ProtocolError::new(
                                    "test",
                                    "nested request failed"
                                ))?
                                .revision,
                            1
                        );
                        Ok(InteractionOutcome::Opened)
                    }
                })),
            )
            .await?;
        assert_eq!(status.attempt_id, attempt);
        assert!(status.authenticated);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "sequential reverse requests must work"
        );
        let id = observer.send("config.get", Value::Null).await?;
        assert_eq!(
            observer.result(id).await?["result"]["revision"],
            1,
            "the other connection must not receive interaction.request"
        );
        let status = client
            .authenticate(command(AuthOperation::Status, 1), None)
            .await?;
        assert!(status.authenticated);
        redacted(&serde_json::to_string(&status)?);
        let run = client
            .start_run(session, "ordinary model request".into())
            .await?;
        completed(&terminal(&mut events, run).await?);
        // Replay is production history; the intentionally sensitive Provider audit is not.
        let head = client.subscribe(session, 0).await?;
        for _ in 0..head {
            redacted(&serde_json::to_string(
                &events.recv().await.ok_or("missing replay")?,
            )?);
        }
        let snapshot = client.config().await?;
        redacted(&serde_json::to_string(&snapshot)?);
        client
            .apply_config(1, inputs.config("login", Some("other-key"), None)?)
            .await?;
        client
            .authenticate(command(AuthOperation::Status, 2), None)
            .await?;
        let records = inputs.records()?;
        let auths = requests(&records, "provider.auth");
        assert_eq!(auths.len(), 3);
        assert!(
            auths[0]["message"]["params"]["credential"].is_null(),
            "login permits an empty selected slot"
        );
        assert_eq!(auths[1]["message"]["params"]["credential"], LOGIN);
        assert_eq!(
            auths[2]["message"]["params"]["credential"], OTHER,
            "replacement cannot touch another slot"
        );
        let steps = requests(&records, "provider.step");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["message"]["params"]["credential"], LOGIN);
        assert_ne!(steps[0]["instance_id"], auths[0]["instance_id"]);
        for auth in &auths[0..2] {
            assert_eq!(auth["ppid"], server.pid);
        }
        assert!(!serde_json::to_string(&auths[0..2])?.contains(OTHER));
        assert!(
            !serde_json::to_string(&steps[0]["message"]["params"]["messages"])?.contains("auth-")
        );
        assert!(
            !serde_json::to_string(&requests(&records, "provider.validate"))?
                .contains("auth-secret-")
        );
        let writes = requests(&records, "host.credential.replace");
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["message"]["params"], json!({"credential":LOGIN}));
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn declined_and_unavailable_login_keep_existing_credentials() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, _) = Client::connect(&inputs.endpoint).await?;
        client.put_secret("selected-key", OLD).await?;
        client
            .apply_config(0, inputs.config("login", Some("selected-key"), None)?)
            .await?;
        for (callback, code) in [
            (
                Some(Interaction::new(|_| async {
                    Ok(InteractionOutcome::Declined)
                })),
                "auth_declined",
            ),
            (
                Some(Interaction::new(|_| async {
                    Ok(InteractionOutcome::Unavailable)
                })),
                "interaction_unavailable",
            ),
            (None, "interaction_unavailable"),
        ] {
            remote(
                client
                    .authenticate(command(AuthOperation::Login, 1), callback)
                    .await
                    .expect_err("not authenticated by presentation"),
                code,
            );
        }
        client
            .authenticate(command(AuthOperation::Status, 1), None)
            .await?;
        let records = inputs.records()?;
        assert!(requests(&records, "host.credential.replace").is_empty());
        assert!(
            requests(&records, "provider.auth")
                .iter()
                .all(|record| record["message"]["params"]["credential"] == OLD)
        );
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn wrong_attempts_urls_and_nonmonotonic_provider_ids_fail_closed() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, _) = Client::connect(&inputs.endpoint).await?;
        client.put_secret("selected-key", OLD).await?;
        let scenarios = [
            ("wrong_status", "provider_protocol"),
            ("wrong_interaction", "provider_protocol"),
            ("http_url", "provider_protocol"),
            ("control_url", "provider_protocol"),
            ("oversized_url", "provider_protocol"),
            ("zero_host_id", "provider_protocol"),
            ("repeat_host_id", "provider_protocol"),
            ("error", "auth_failed"),
            ("unknown_auth", "auth_unsupported"),
        ];
        for (index, (scenario, code)) in scenarios.into_iter().enumerate() {
            let revision = index as u64 + 1;
            client
                .apply_config(
                    revision - 1,
                    inputs.config(scenario, Some("selected-key"), None)?,
                )
                .await?;
            remote(
                client
                    .authenticate(command(AuthOperation::Login, revision), Some(opened()))
                    .await
                    .expect_err("invalid auth must fail"),
                code,
            );
        }
        let records = inputs.records()?;
        assert!(requests(&records, "host.credential.replace").is_empty());
        assert_eq!(
            requests(&records, "provider.auth").len(),
            scenarios.len(),
            "no automatic retry"
        );
        let rejected: Vec<_> = records
            .iter()
            .filter(|record| {
                record["direction"] == "received"
                    && record["message"]["error"]["code"] == "provider_protocol"
            })
            .collect();
        assert_eq!(
            rejected.len(),
            4,
            "wrong attempt and unsafe URLs rejected by the Server host service"
        );
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn cancellation_before_acceptance_never_launches_a_delayed_login() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, _) = Client::connect(&inputs.endpoint).await?;
        client
            .apply_config(0, inputs.config("login", Some("selected-key"), None)?)
            .await?;
        let delayed = command(AuthOperation::Login, 1);
        remote(
            client
                .cancel_auth(delayed.attempt_id)
                .await
                .expect_err("not active yet"),
            "not_active",
        );
        remote(
            client
                .authenticate(delayed, Some(opened()))
                .await
                .expect_err("cancelled before acceptance"),
            "auth_cancelled",
        );
        assert!(requests(&inputs.records()?, "provider.auth").is_empty());
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn attempt_reuse_config_revision_and_no_selected_reference_are_rejected_before_launch()
-> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        client.apply_config(0, inputs.config("login", Some("selected-key"), None)?).await?;
        let auth = command(AuthOperation::Status, 1);
        let status = client.authenticate(auth.clone(), None).await?;
        assert!(!status.authenticated);
        let before = inputs.records()?.len();
        remote(client.authenticate(auth.clone(), None).await.expect_err("attempt reuse forbidden"), "invalid_params");
        remote(client.authenticate(command(AuthOperation::Login, 0), Some(opened())).await.expect_err("stale config"), "revision_conflict");
        remote(client.cancel_auth(auth.attempt_id).await.expect_err("completed auth not active"), "not_active");
        assert_eq!(inputs.records()?.len(), before);
        // Attempt identities are connection-scoped, not globally reserved.
        let (other, _) = Client::connect(&inputs.endpoint).await?;
        other.authenticate(auth, None).await?;
        let before = inputs.records()?.len();
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let run = client.start_run(session, "missing credential".into()).await?;
        let seen = terminal(&mut events, run).await?;
        assert!(matches!(&seen.last().ok_or("missing terminal")?.kind, EventKind::RunFailed { error, .. } if error.code == "secret_not_found"));
        assert_eq!(inputs.records()?.len(), before, "missing model credential must not launch a child");
        client.apply_config(1, inputs.config("login", None, None)?).await?;
        let before = inputs.records()?.len();
        for operation in [AuthOperation::Login, AuthOperation::Status, AuthOperation::Logout] {
            remote(client.authenticate(command(operation, 2), Some(opened())).await.expect_err("auth requires selected reference"), "invalid_config");
        }
        assert_eq!(inputs.records()?.len(), before);
        server.stop().await?;
        Ok(())
    }).await
}

#[tokio::test]
async fn validation_status_and_unselected_model_have_no_credential_write_authority() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, mut events) = Client::connect(&inputs.endpoint).await?;
        client.put_secret("selected-key", OLD).await?;
        client.validate_config(&inputs.config("validate_write", Some("selected-key"), None)?).await?;
        assert_eq!(client.config().await?.revision, 0);
        client.apply_config(0, inputs.config("status_write", Some("selected-key"), None)?).await?;
        assert!(client.authenticate(command(AuthOperation::Status, 1), Some(opened())).await?.authenticated);
        client.apply_config(1, inputs.config("status_interact", Some("selected-key"), None)?).await?;
        client.authenticate(command(AuthOperation::Status, 2), Some(Interaction::new(|_| async { panic!("status must never invoke interaction") }))).await?;
        client.apply_config(2, inputs.config("step_write", None, None)?).await?;
        let session = client.create_session().await?;
        client.subscribe(session, 0).await?;
        let run = client.start_run(session, "no selected scope".into()).await?;
        completed(&terminal(&mut events, run).await?);
        client.apply_config(3, inputs.config("step_interact", Some("selected-key"), None)?).await?;
        let run = client.start_run(session, "no model interaction".into()).await?;
        let seen = terminal(&mut events, run).await?;
        assert!(matches!(&seen.last().ok_or("missing terminal")?.kind, EventKind::RunFailed { error, .. } if error.code == "auth_required"));
        client.authenticate(command(AuthOperation::Status, 4), None).await?;
        let records = inputs.records()?;
        let writes = requests(&records, "host.credential.replace");
        assert_eq!(writes.len(), 3);
        for write in writes {
            let replies: Vec<_> = records.iter().filter(|record| record["instance_id"] == write["instance_id"] && record["message"]["type"] == "error" && record["message"]["id"] == write["message"]["id"]).collect();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0]["message"]["error"]["code"], "host_service_unavailable");
        }
        assert!(requests(&records, "provider.auth").iter().all(|record| record["message"]["params"]["credential"] == OLD));
        assert!(requests(&records, "provider.validate").iter().all(|record| record["message"]["params"].get("credential").is_none()));
        assert_eq!(requests(&records, "provider.step")[0]["message"]["params"]["credential"], Value::Null);
        assert_eq!(records.iter().filter(|record| record["message"]["error"]["code"] == "interaction_unavailable").count(), 2);
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    }).await
}

#[tokio::test]
async fn cancel_stale_reply_wrong_client_reply_and_disconnect_fence_auth_authority() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (admin, _) = Client::connect(&inputs.endpoint).await?;
        admin.put_secret("selected-key", OLD).await?;
        admin
            .apply_config(
                0,
                inputs.config(
                    "pending",
                    Some("selected-key"),
                    Some(listener.local_addr()?.port()),
                )?,
            )
            .await?;
        let mut raw = StreamClient::connect(&inputs.endpoint).await?;
        let auth = command(AuthOperation::Login, 1);
        let auth_id = raw
            .send("provider.auth", serde_json::to_value(&auth)?)
            .await?;
        let (mut child, _) = gate(&listener).await?;
        let interaction = raw.next().await?;
        assert_eq!(interaction["method"], "interaction.request");
        assert_eq!(interaction["params"]["attempt_id"], json!(auth.attempt_id));
        remote(
            admin
                .cancel_auth(auth.attempt_id)
                .await
                .expect_err("only initiating connection may cancel"),
            "not_active",
        );
        let duplicate = raw
            .send("provider.auth", serde_json::to_value(&auth)?)
            .await?;
        assert_eq!(
            raw.result(duplicate).await?["error"]["code"],
            "invalid_params",
            "an active attempt cannot be reused"
        );
        let wrong_cancel = raw
            .send("auth.cancel", json!({"attempt_id":AuthAttemptId::new()}))
            .await?;
        assert_eq!(
            raw.result(wrong_cancel).await?["error"]["code"],
            "not_active"
        );
        let cancel_id = raw
            .send("auth.cancel", json!({"attempt_id":auth.attempt_id}))
            .await?;
        let first = raw.next().await?;
        let second = raw.next().await?;
        for message in [&first, &second] {
            redacted(&serde_json::to_string(message)?);
            if message["id"] == auth_id {
                assert_eq!(message["error"]["code"], "auth_cancelled");
            } else {
                assert_eq!(message["id"], cancel_id);
                assert_eq!(message["type"], "response");
            }
        }
        assert_ne!(first["id"], second["id"]);
        child_stopped(&mut child).await?;
        // A previously correlated reverse reply is now stale. It cannot revive a
        // cancelled attempt or write into the next fresh child's credential scope.
        raw.reply(
            interaction["id"].as_u64().ok_or("missing interaction id")?,
            json!({"attempt_id":auth.attempt_id,"outcome":"opened"}),
        )
        .await?;
        let id = raw
            .send(
                "provider.auth",
                serde_json::to_value(command(AuthOperation::Status, 1))?,
            )
            .await?;
        assert_eq!(raw.result(id).await?["result"]["authenticated"], true);

        let auth = command(AuthOperation::Login, 1);
        let id = raw
            .send("provider.auth", serde_json::to_value(&auth)?)
            .await?;
        let (mut child, _) = gate(&listener).await?;
        let current_interaction = raw.next().await?;
        assert_ne!(interaction["id"], current_interaction["id"]);
        raw.reply(
            interaction["id"].as_u64().ok_or("old interaction id")?,
            json!({"attempt_id":auth.attempt_id,"outcome":"opened"}),
        )
        .await?;
        let barrier = raw.send("config.get", Value::Null).await?;
        assert_eq!(
            raw.result(barrier).await?["result"]["revision"],
            1,
            "a stale reverse reply must not complete a new live attempt"
        );
        raw.reply(
            current_interaction["id"]
                .as_u64()
                .ok_or("missing interaction id")?,
            json!({"attempt_id":AuthAttemptId::new(),"outcome":"opened"}),
        )
        .await?;
        assert_eq!(raw.result(id).await?["error"]["code"], "provider_protocol");
        child_stopped(&mut child).await?;

        let id = raw
            .send(
                "provider.auth",
                serde_json::to_value(command(AuthOperation::Login, 1))?,
            )
            .await?;
        assert!(id > 0);
        let (mut child, _) = gate(&listener).await?;
        assert_eq!(raw.next().await?["method"], "interaction.request");
        drop(raw);
        child_stopped(&mut child).await?;
        assert!(
            admin
                .authenticate(command(AuthOperation::Status, 1), None)
                .await?
                .authenticated
        );
        let records = inputs.records()?;
        assert!(requests(&records, "host.credential.replace").is_empty());
        assert!(
            requests(&records, "provider.auth")
                .iter()
                .all(|record| record["message"]["params"]["credential"] == OLD)
        );
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

struct CallbackLifetime {
    dropped: Arc<AtomicUsize>,
    released: Arc<Notify>,
}
impl Drop for CallbackLifetime {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        self.released.notify_one();
    }
}

#[tokio::test]
async fn sdk_auth_future_drop_aborts_callback_and_opened_is_not_authentication() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, _) = Client::connect(&inputs.endpoint).await?;
        client.put_secret("selected-key", OLD).await?;
        client
            .apply_config(
                0,
                inputs.config(
                    "pending",
                    Some("selected-key"),
                    Some(listener.local_addr()?.port()),
                )?,
            )
            .await?;
        let dropped = Arc::new(AtomicUsize::new(0));
        let lifetime = dropped.clone();
        let released = Arc::new(Notify::new());
        let release_signal = released.clone();
        let (entered, entry) = oneshot::channel();
        let entered = Arc::new(std::sync::Mutex::new(Some(entered)));
        let callback = Interaction::new(move |_| {
            let lifetime = CallbackLifetime {
                dropped: lifetime.clone(),
                released: release_signal.clone(),
            };
            entered
                .lock()
                .expect("test signal lock")
                .take()
                .expect("one callback")
                .send(())
                .expect("entry receiver exists");
            async move {
                let _lifetime = lifetime;
                std::future::pending().await
            }
        });
        let owned = client.clone();
        let auth = tokio::spawn(async move {
            owned
                .authenticate(command(AuthOperation::Login, 1), Some(callback))
                .await
        });
        let (mut child, _) = gate(&listener).await?;
        entry.await?;
        auth.abort();
        assert!(auth.await.expect_err("auth task cancelled").is_cancelled());
        child_stopped(&mut child).await?;
        released.notified().await;
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "pending presentation callback released"
        );
        // Presentation alone must not authenticate: the Provider still awaits its
        // own verification, represented here by a gate before credential replace.
        let auth = command(AuthOperation::Login, 1);
        let attempt = auth.attempt_id;
        let owned = client.clone();
        let task = tokio::spawn(async move { owned.authenticate(auth, Some(opened())).await });
        let (mut child, _) = gate(&listener).await?;
        let mut line = String::new();
        assert_ne!(child.read_line(&mut line).await?, 0);
        assert_eq!(serde_json::from_str::<Value>(&line)?["phase"], "presented");
        client.cancel_auth(attempt).await?;
        remote(
            task.await?
                .expect_err("cancelled before Provider verification"),
            "auth_cancelled",
        );
        child_stopped(&mut child).await?;
        client
            .authenticate(command(AuthOperation::Status, 1), None)
            .await?;
        let records = inputs.records()?;
        assert!(requests(&records, "host.credential.replace").is_empty());
        assert_eq!(
            requests(&records, "provider.auth")
                .last()
                .ok_or("status request")?["message"]["params"]["credential"],
            OLD
        );
        server.stop().await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn acknowledged_credential_replacement_survives_cancel_and_logout_clears_only_scope()
-> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (client, _) = Client::connect(&inputs.endpoint).await?;
        client.put_secret("other-key", OTHER).await?;
        client
            .apply_config(
                0,
                inputs.config(
                    "commit_then_gate",
                    Some("selected-key"),
                    Some(listener.local_addr()?.port()),
                )?,
            )
            .await?;
        let auth = command(AuthOperation::Login, 1);
        let attempt = auth.attempt_id;
        let owned = client.clone();
        let task = tokio::spawn(async move { owned.authenticate(auth, None).await });
        let (mut child, ready) = gate(&listener).await?;
        assert_eq!(ready["phase"], "committed");
        client.cancel_auth(attempt).await?;
        remote(
            task.await?.expect_err("cancelled after commit"),
            "auth_cancelled",
        );
        child_stopped(&mut child).await?;
        assert!(
            client
                .authenticate(command(AuthOperation::Status, 1), None)
                .await?
                .authenticated
        );
        let status = client
            .authenticate(command(AuthOperation::Logout, 1), None)
            .await?;
        assert!(!status.authenticated);
        assert_eq!(status.revocation_confirmed, Some(false));
        assert!(
            !client
                .authenticate(command(AuthOperation::Status, 1), None)
                .await?
                .authenticated
        );
        client
            .apply_config(1, inputs.config("login", Some("other-key"), None)?)
            .await?;
        assert!(
            client
                .authenticate(command(AuthOperation::Status, 2), None)
                .await?
                .authenticated
        );
        let records = inputs.records()?;
        let auths = requests(&records, "provider.auth");
        assert_eq!(
            auths[1]["message"]["params"]["credential"], LOGIN,
            "host ack is an immediate commit, not rolled back by cancel"
        );
        assert!(auths[3]["message"]["params"]["credential"].is_null());
        assert_eq!(auths[4]["message"]["params"]["credential"], OTHER);
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn refresh_serializes_model_auth_and_client_secret_updates_per_reference() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut server = AuditedServer::spawn(&inputs.endpoint, &inputs.stderr).await?;
        let (first, mut first_events) = Client::connect(&inputs.endpoint).await?;
        let (second, mut second_events) = Client::connect(&inputs.endpoint).await?;
        first.put_secret("selected-key", OLD).await?;
        first
            .apply_config(
                0,
                inputs.config(
                    "refresh",
                    Some("selected-key"),
                    Some(listener.local_addr()?.port()),
                )?,
            )
            .await?;
        let one = first.create_session().await?;
        let two = second.create_session().await?;
        first.subscribe(one, 0).await?;
        second.subscribe(two, 0).await?;
        let run_one = first.start_run(one, "first refresh".into()).await?;
        let (mut first_child, ready_one) = gate(&listener).await?;
        assert_eq!(ready_one["request"]["params"]["credential"], OLD);
        let run_two = second.start_run(two, "second refresh".into()).await?;
        loop {
            let event = second_events
                .recv()
                .await
                .ok_or("missing second model authorization")?;
            if matches!(event.kind, EventKind::ModelCallStarted { .. }) {
                break;
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(40), listener.accept())
                .await
                .is_err(),
            "a second operation on the same reference must not launch while refresh holds the lease"
        );
        // Both calls are accepted before releasing the first child. The second
        // fresh child must receive generation 1, not independently refresh OLD.
        first_child.get_mut().write_all(b"+").await?;
        completed(&terminal(&mut first_events, run_one).await?);
        child_stopped(&mut first_child).await?;
        let (mut second_child, ready_two) = gate(&listener).await?;
        let credential: Value = serde_json::from_str(
            ready_two["request"]["params"]["credential"]
                .as_str()
                .ok_or("second credential")?,
        )?;
        assert_eq!(credential["generation"], 1);
        assert_ne!(ready_one["instance_id"], ready_two["instance_id"]);
        let owned = first.clone();
        let mut status = tokio::spawn(async move {
            owned
                .authenticate(command(AuthOperation::Status, 1), None)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(40), &mut status)
                .await
                .is_err(),
            "status shares the credential lease"
        );
        // Other references are independent; this completes while refresh is gated.
        first.put_secret("unselected-key", OTHER).await?;
        let owned = second.clone();
        let mut put = tokio::spawn(async move { owned.put_secret("selected-key", LOGIN).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(40), &mut put)
                .await
                .is_err(),
            "Client writes share the credential lease"
        );
        second_child.get_mut().write_all(b"+").await?;
        completed(&terminal(&mut second_events, run_two).await?);
        child_stopped(&mut second_child).await?;
        assert!(status.await??.authenticated);
        put.await??;
        first
            .authenticate(command(AuthOperation::Status, 1), None)
            .await?;
        let records = inputs.records()?;
        let auths = requests(&records, "provider.auth");
        let during: Value = serde_json::from_str(
            auths[0]["message"]["params"]["credential"]
                .as_str()
                .ok_or("status credential")?,
        )?;
        assert_eq!(
            during["generation"], 2,
            "status observes completed refresh, before queued Client replacement"
        );
        assert_eq!(auths[1]["message"]["params"]["credential"], LOGIN);
        assert_eq!(requests(&records, "host.credential.replace").len(), 2);
        server.stop().await?;
        redacted(&fs::read_to_string(&inputs.stderr)?);
        Ok(())
    })
    .await
}
