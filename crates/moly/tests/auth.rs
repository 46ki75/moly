//! CLI auth UX and ownership checks against a fake protocol Server, not live OAuth.
#![cfg(unix)]
use moly_client::protocol::{SERVER_VERSION, ServerId};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixListener,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    process::{Child, ChildStdin, ChildStdout, Command},
};
type TestError = Box<dyn std::error::Error + Send + Sync>;
struct Cli {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
impl Cli {
    fn spawn(endpoint: &str, directory: &std::path::Path) -> Result<Self, TestError> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_moly"))
            .args(["--connect", endpoint])
            .env_clear()
            .env("MOLY_PROVIDER", "openai-codex")
            .env("MOLY_MODEL", "explicit-model")
            .env("MOLY_AUTH_STATE_FILE", directory.join("auth.json"))
            .env("MOLY_API_KEY", "must-never-be-sent\n")
            .env("RUST_LOG", "off")
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().ok_or("missing stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("missing stdout")?);
        Ok(Self {
            child,
            input: Some(input),
            output,
        })
    }
    async fn send(&mut self, text: &str) -> Result<(), TestError> {
        self.input
            .as_mut()
            .ok_or("stdin closed")?
            .write_all(text.as_bytes())
            .await?;
        Ok(())
    }
    async fn prompt(&mut self) -> Result<String, TestError> {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"moly> ") {
            bytes.push(self.output.read_u8().await?);
            if bytes.len() > 16384 {
                return Err("unbounded CLI output".into());
            }
        }
        Ok(String::from_utf8(bytes)?)
    }
    async fn line(&mut self) -> Result<String, TestError> {
        let mut line = String::new();
        if self.output.read_line(&mut line).await? == 0 {
            return Err("unexpected stdout EOF".into());
        }
        Ok(line)
    }
    async fn finish(mut self, quit: bool) -> Result<String, TestError> {
        if quit {
            self.send("/quit\n").await?;
        }
        self.input.take();
        let mut stderr = self.child.stderr.take().ok_or("missing stderr")?;
        let mut diagnostic = String::new();
        let (status, _) =
            tokio::try_join!(self.child.wait(), stderr.read_to_string(&mut diagnostic))?;
        assert!(status.success(), "CLI failed: {diagnostic}");
        assert!(!diagnostic.contains("must-never-be-sent"));
        Ok(diagnostic)
    }
}
struct Fake {
    input: BufReader<OwnedReadHalf>,
    output: OwnedWriteHalf,
}
impl Fake {
    async fn next(&mut self) -> Result<Value, TestError> {
        let mut line = String::new();
        if self.input.read_line(&mut line).await? == 0 {
            return Err("unexpected connection EOF".into());
        }
        Ok(serde_json::from_str(&line)?)
    }
    async fn request(&mut self, method: &str) -> Result<(u64, Value), TestError> {
        let value = self.next().await?;
        assert_eq!(value["type"], "request");
        assert_eq!(value["method"], method);
        Ok((
            value["id"].as_u64().ok_or("missing request id")?,
            value["params"].clone(),
        ))
    }
    async fn send(&mut self, value: Value) -> Result<(), TestError> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.output.write_all(&bytes).await?;
        self.output.flush().await?;
        Ok(())
    }
    async fn respond(&mut self, id: u64, result: Value) -> Result<(), TestError> {
        self.send(json!({"version":1,"type":"response","id":id,"result":result}))
            .await
    }
    async fn handshake(&mut self) -> Result<(), TestError> {
        let (id, params) = self.request("initialize").await?;
        assert_eq!(params["protocol_version"], SERVER_VERSION);
        self.respond(
            id,
            json!({"server_id":ServerId::new(),"role":"server","protocol_version":SERVER_VERSION}),
        )
        .await
    }
}
async fn fixture(command: &str) -> Result<(tempfile::TempDir, Cli, Fake, Value), TestError> {
    let directory = tempfile::tempdir()?;
    let endpoint = directory.path().join("server.sock");
    let listener = UnixListener::bind(&endpoint)?;
    let mut cli = Cli::spawn(
        endpoint.to_str().ok_or("invalid endpoint")?,
        directory.path(),
    )?;
    assert_eq!(cli.prompt().await?, "moly> ");
    cli.send(command).await?;
    let (stream, _) = listener.accept().await?;
    let (input, output) = stream.into_split();
    let mut fake = Fake {
        input: BufReader::new(input),
        output,
    };
    fake.handshake().await?;
    let (id, _) = fake.request("config.get").await?;
    fake.respond(id, json!({"revision":0,"config":null}))
        .await?;
    let (id, config) = fake.request("config.validate").await?;
    assert_eq!(config["secret_ref"], "cli-provider");
    assert_eq!(config["provider"]["options"]["model"], "explicit-model");
    assert!(
        config["provider"]["options"]["host_id"]
            .as_str()
            .is_some_and(|host| host.starts_with("urn:uuid:"))
    );
    let executable = std::path::Path::new(
        config["provider"]["command"]["executable"]
            .as_str()
            .ok_or("missing executable")?,
    );
    assert_eq!(
        executable.file_name().and_then(|name| name.to_str()),
        Some("moly-provider-openai-codex")
    );
    assert!(config.to_string().find("must-never-be-sent").is_none());
    fake.respond(id, Value::Null).await?;
    // No secret.put is allowed in the OAuth profile, despite ambient MOLY_API_KEY.
    let (id, params) = fake.request("config.apply").await?;
    assert_eq!(params["base_revision"], 0);
    assert_eq!(params["config"], config);
    fake.respond(id, json!({"revision":1,"config":config}))
        .await?;
    Ok((directory, cli, fake, config))
}
fn status(attempt: Value, registration: Option<Value>) -> Value {
    json!({"attempt_id":attempt,"authenticated":true,"registration":registration,"revocation_confirmed":null})
}
async fn present(cli: &mut Cli, fake: &mut Fake, attempt: Value) -> Result<(), TestError> {
    fake.send(json!({"version":1,"type":"request","id":1,"method":"interaction.request","params":{"attempt_id":attempt,"url":"https://auth.example.test/authorize?state=opaque&code_challenge=pkce"}})).await?;
    assert_eq!(cli.line().await?, "Open this HTTPS URL in your browser:\n");
    assert_eq!(
        cli.line().await?,
        "https://auth.example.test/authorize?state=opaque&code_challenge=pkce\n"
    );
    let response = fake.next().await?;
    assert_eq!(response["type"], "response");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["attempt_id"], attempt);
    assert_eq!(response["result"]["outcome"], "opened");
    Ok(())
}

#[tokio::test]
async fn login_status_and_logout_are_lazy_first_operations_not_sessions() -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        for (text, operation) in [
            ("/login\n", "login"),
            ("/auth\n", "status"),
            ("/logout\n", "logout"),
        ] {
            let (_directory, mut cli, mut fake, _) = fixture(text).await?;
            let (id, auth) = fake.request("provider.auth").await?;
            assert_eq!(auth["operation"], operation);
            assert_eq!(auth["config_revision"], 1);
            if operation == "login" {
                present(&mut cli, &mut fake, auth["attempt_id"].clone()).await?;
            }
            fake.respond(id, status(auth["attempt_id"].clone(), None))
                .await?;
            assert_eq!(cli.prompt().await?, "Authentication: signed in\nmoly> ");
            let stderr = cli.finish(true).await?;
            assert!(
                !stderr.contains("; session"),
                "auth must not allocate a conversation"
            );
        }
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn configured_unrelated_providers_never_supply_managed_local_registration()
-> Result<(), TestError> {
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
    tokio::time::timeout(Duration::from_secs(10), async {
        for (same_binary, conflicting_registration) in [(false, false), (true, false), (true, true)] {
            let directory = tempfile::tempdir()?;
            let endpoint = directory.path().join("server.sock");
            let listener = UnixListener::bind(&endpoint)?;
            let path = directory.path().join("auth.json");
            let mut local = json!({"host_id":"urn:uuid:00000000-0000-4000-8000-000000000001"});
            if conflicting_registration { local["registration"] = json!({"client_id":"saved-account"}); }
            OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?.write_all(local.to_string().as_bytes())?;
            let mut cli = Cli::spawn(endpoint.to_str().ok_or("invalid endpoint")?, directory.path())?;
            cli.prompt().await?;
            cli.send("/auth\n").await?;
            let (stream, _) = listener.accept().await?;
            let (input, output) = stream.into_split();
            let mut fake = Fake { input: BufReader::new(input), output };
            fake.handshake().await?;
            let (id, _) = fake.request("config.get").await?;
            let executable = if same_binary {
                std::path::Path::new(env!("CARGO_BIN_EXE_moly")).with_file_name("moly-provider-openai-codex")
            } else { directory.path().join("another-provider") };
            let mut config = json!({"workspace":directory.path(),"secret_ref":"cli-provider","provider":{
                "command":{"executable":executable,"args":[],"env":{"RUST_LOG":"off"}},
                "options":{"model":"explicit-model","host_id":"urn:uuid:00000000-0000-4000-8000-000000000002"}
            }});
            if conflicting_registration {
                config["provider"]["options"]["host_id"] = local["host_id"].clone();
                config["provider"]["options"]["registration"] = json!({"client_id":"other-account"});
            }
            fake.respond(id, json!({"revision":9,"config":config})).await?;
            let (id, auth) = fake.request("provider.auth").await?;
            assert_eq!(auth["config_revision"], 9);
            let returned = if conflicting_registration { json!({"client_id":"other-account"}) } else { json!({"client_id":"foreign","access_token":"must-not-be-adopted"}) };
            fake.respond(id, status(auth["attempt_id"].clone(), Some(returned))).await?;
            assert_eq!(cli.prompt().await?, "Authentication: signed in\nmoly> ");
            assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(path)?)?, local);
            let diagnostic = cli.finish(true).await?;
            assert!(!diagnostic.contains("must-not-be-adopted"));
        }
        Ok::<_, TestError>(())
    }).await?
}

#[tokio::test]
async fn ctrl_c_and_eof_cancel_on_same_connection_and_await_auth_terminal() -> Result<(), TestError>
{
    tokio::time::timeout(Duration::from_secs(10), async {
        for intent in ["ctrl_c", "eof", "quit"] {
            let (_directory, mut cli, mut fake, _) = fixture("/login\n").await?;
            let (id, auth) = fake.request("provider.auth").await?;
            present(&mut cli, &mut fake, auth["attempt_id"].clone()).await?;
            if intent == "eof" { cli.input.take(); } else if intent == "quit" {
                cli.send("/quit\n").await?;
            } else {
                let pid = cli.child.id().ok_or("missing pid")?;
                assert!(Command::new("kill").args(["-INT", &pid.to_string()]).status().await?.success());
            }
            let (cancel_id, params) = fake.request("auth.cancel").await?;
            assert_eq!(params["attempt_id"], auth["attempt_id"]);
            fake.respond(cancel_id, Value::Null).await?;
            assert!(tokio::time::timeout(Duration::from_millis(80), cli.output.read_u8()).await.is_err(), "cancel ACK alone must not return to prompt or exit");
            let code = if intent == "quit" { "auth_failed" } else { "auth_cancelled" };
            fake.send(json!({"version":1,"type":"error","id":id,"error":{"code":code,"message":"Authentication ended"}})).await?;
            if intent == "ctrl_c" { assert_eq!(cli.prompt().await?, "moly> "); }
            let diagnostic = cli.finish(intent == "ctrl_c").await?;
            assert!(diagnostic.contains(if intent == "quit" { "auth_failed" } else { "Authentication cancelled." }));
        }
        Ok::<_, TestError>(())
    }).await?
}

#[tokio::test]
async fn status_recovers_registration_locally_and_uses_cas_without_overwriting_new_config()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        for conflict in [false, true] {
            let (directory, mut cli, mut fake, config) = fixture("/auth\n").await?;
            let (auth_id, auth) = fake.request("provider.auth").await?;
            let registration = json!({"version":1,"client_id":"oaiapp_issued","subject":"nonsecret-account","issuer":"https://auth.openai.com","host_id":config["provider"]["options"]["host_id"]});
            fake.respond(auth_id, status(auth["attempt_id"].clone(), Some(registration.clone()))).await?;
            let (id, _) = fake.request("config.get").await?;
            let mut current = config.clone();
            if conflict { current["provider"]["options"]["model"] = json!("another-client-model"); }
            fake.respond(id, json!({"revision":if conflict { 2 } else { 1 },"config":current})).await?;
            if !conflict {
                let (id, apply) = fake.request("config.apply").await?;
                assert_eq!(apply["base_revision"], 1);
                let mut intended = config.clone(); intended["provider"]["options"]["registration"] = registration.clone();
                assert_eq!(apply["config"], intended);
                fake.respond(id, json!({"revision":2,"config":intended})).await?;
                assert_eq!(cli.prompt().await?, "Authentication: signed in\nmoly> ");
            } else { assert_eq!(cli.prompt().await?, "moly> "); }
            let persisted: Value = serde_json::from_slice(&std::fs::read(directory.path().join("auth.json"))?)?;
            assert_eq!(persisted["registration"], registration);
            assert_eq!(persisted.as_object().ok_or("invalid state")?.len(), 2);
            let stderr = cli.finish(true).await?;
            if conflict { assert!(stderr.contains("Agent Server config changed; not overwritten")); }
        }
        Ok::<_, TestError>(())
    }).await?
}
