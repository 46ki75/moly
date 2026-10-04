//! Product REPL checks with real processes and a deterministic local HTTP provider.
use crate::server_process::ServerProcess;
use moly_client::{
    Client,
    protocol::{ResolvedConfig, SessionId},
};
use serde_json::{Value, json};
use std::{io, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
};

type TestError = Box<dyn std::error::Error + Send + Sync>;

struct Cli {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
}
impl Cli {
    fn spawn(endpoint: &str, directory: &std::path::Path) -> Result<Self, TestError> {
        Self::spawn_with_env::<&str>(endpoint, directory, &[])
    }

    fn spawn_with_env<V: AsRef<std::ffi::OsStr>>(
        endpoint: &str,
        directory: &std::path::Path,
        environment: &[(&str, V)],
    ) -> Result<Self, TestError> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_moly"));
        command
            .args(["--connect", endpoint])
            .env_clear()
            // A configured Server wins over all of these invalid ambient settings.
            .env("MOLY_MODEL_ENDPOINT", "not a URL")
            .env("MOLY_MODEL", "")
            .env("MOLY_API_KEY", "must-not-be-used\n")
            .env("RUST_LOG", "off")
            .envs(environment.iter().map(|(key, value)| (*key, value)))
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let mut child = command.spawn()?;
        Ok(Self {
            stdin: child.stdin.take(),
            stdout: BufReader::new(child.stdout.take().ok_or("missing stdout")?),
            stderr: BufReader::new(child.stderr.take().ok_or("missing stderr")?),
            child,
        })
    }

    async fn send(&mut self, text: &str) -> io::Result<()> {
        self.stdin
            .as_mut()
            .expect("test stdin is open")
            .write_all(text.as_bytes())
            .await
    }

    async fn prompt(&mut self) -> Result<String, TestError> {
        let mut output = Vec::new();
        while !output.ends_with(b"moly> ") {
            output.push(self.stdout.read_u8().await?);
            assert!(output.len() <= 1024 * 1024, "unbounded REPL output");
        }
        Ok(String::from_utf8(output)?)
    }

    async fn diagnostic(&mut self) -> Result<String, TestError> {
        let mut line = String::new();
        assert_ne!(
            self.stderr.read_line(&mut line).await?,
            0,
            "missing diagnostic"
        );
        Ok(line)
    }

    async fn session(&mut self, server: &ServerProcess) -> Result<SessionId, TestError> {
        let line = self.diagnostic().await?;
        assert!(line.starts_with(&format!("Agent Server {} at ", server.server_id)));
        let (_, id) = line
            .trim_end()
            .rsplit_once("; session ")
            .ok_or("missing session")?;
        Ok(serde_json::from_value(json!(id))?)
    }

    async fn finish(mut self) -> Result<(String, String), TestError> {
        self.stdin.take();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let (status, _, _) = tokio::try_join!(
            self.child.wait(),
            self.stdout.read_to_string(&mut stdout),
            self.stderr.read_to_string(&mut stderr),
        )?;
        assert!(status.success(), "CLI failed: {stderr}");
        Ok((stdout, stderr))
    }
}

async fn request(listener: &TcpListener) -> Result<(Value, TcpStream), TestError> {
    let (headers, body, stream) = http_request(listener).await?;
    assert!(headers.starts_with("POST /chat/completions HTTP/1.1\r\n"));
    assert!(!headers.to_ascii_lowercase().contains("authorization:"));
    assert!(!headers.to_ascii_lowercase().contains("x-opencode-session:"));
    assert_eq!(body["model"], "repl-mock");
    Ok((body, stream))
}

async fn http_request(listener: &TcpListener) -> Result<(String, Value, TcpStream), TestError> {
    let (stream, _) = listener.accept().await?;
    let mut stream = BufReader::new(stream);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Err("incomplete HTTP request".into());
        }
        if line == "\r\n" {
            break;
        }
        headers.push_str(&line);
        assert!(headers.len() < 8192);
    }
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .ok_or("missing content length")?
        .1
        .trim()
        .parse()?;
    assert!(length < 1024 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    let body: Value = serde_json::from_slice(&body)?;
    Ok((headers, body, stream.into_inner()))
}

async fn respond(stream: TcpStream, status: u16, text: &str) -> Result<(), TestError> {
    respond_body(stream, status, json!({"choices":[{"message":{"role":"assistant", "content":text}, "finish_reason":"stop"}]})).await
}

async fn respond_body(mut stream: TcpStream, status: u16, body: Value) -> Result<(), TestError> {
    let body = body.to_string();
    stream.write_all(format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
    ).as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn fixture(
    configured: bool,
) -> Result<
    (
        tempfile::TempDir,
        String,
        ServerProcess,
        TcpListener,
        Client,
    ),
    TestError,
> {
    let directory = tempfile::tempdir()?;
    #[cfg(unix)]
    let endpoint = directory.path().join("s").to_string_lossy().into_owned();
    #[cfg(windows)]
    let endpoint = format!("moly-repl-{}", moly_client::protocol::ConnectionId::new());
    let server = ServerProcess::spawn(&endpoint).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let (client, _events) = Client::connect(&endpoint).await?;
    if configured {
        configure(&client, &listener, directory.path()).await?;
    }
    Ok((directory, endpoint, server, listener, client))
}

#[tokio::test]
async fn repl_keeps_context_recovers_rejections_and_resets_without_stopping_server()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let (directory, endpoint, mut server, listener, observer) = fixture(true).await?;
        let mut cli = Cli::spawn_with_env(
            &endpoint,
            directory.path(),
            &[("MOLY_PROVIDER", "unknown-private-profile")],
        )?;
        assert_eq!(cli.prompt().await?, "moly> ");
        cli.send("hello\n").await?;
        let (body, stream) = request(&listener).await?;
        let old_session = cli.session(&server).await?;
        assert_eq!(
            body["messages"],
            json!([{"role":"user", "content":"hello"}])
        );
        respond(stream, 200, "answer-one").await?;
        assert_eq!(cli.prompt().await?, "answer-one\nmoly> ");

        cli.send("second\n").await?;
        let (body, stream) = request(&listener).await?;
        assert_eq!(
            body["messages"],
            json!([
                {"role":"user", "content":"hello"},
                {"role":"assistant", "content":"answer-one"},
                {"role":"user", "content":"second"},
            ])
        );
        respond(stream, 200, "answer-two").await?;
        assert_eq!(cli.prompt().await?, "answer-two\nmoly> ");

        // Another Client owns the active run; rejection must not end this REPL.
        let busy = observer
            .start_run(old_session, "other client".into())
            .await?;
        let (_, blocked) = request(&listener).await?;
        cli.send("must not be submitted\n").await?;
        assert_eq!(cli.prompt().await?, "moly> ");
        assert!(cli.diagnostic().await?.contains("run_busy"));
        observer.cancel_run(old_session, busy).await?;
        drop(blocked);

        cli.send("/new\n").await?;
        assert_eq!(
            cli.prompt().await?,
            "New conversation on the next message.\nmoly> "
        );
        // Buffered input and EOF must wait for the active run without dropping lines.
        cli.send("//literal\nlast\n").await?;
        cli.stdin.take();
        let (body, stream) = request(&listener).await?;
        let new_session = cli.session(&server).await?;
        assert_ne!(new_session, old_session);
        assert_eq!(
            body["messages"],
            json!([{"role":"user", "content":"/literal"}])
        );
        respond(stream, 200, "fresh-answer").await?;
        let (body, stream) = request(&listener).await?;
        assert_eq!(
            body["messages"],
            json!([
                {"role":"user", "content":"/literal"},
                {"role":"assistant", "content":"fresh-answer"},
                {"role":"user", "content":"last"},
            ])
        );
        respond(stream, 500, "provider-body-must-not-appear").await?;
        let (stdout, stderr) = cli.finish().await?;
        assert_eq!(stdout, "fresh-answer\nmoly> moly> ");
        assert!(stderr.contains("run failed:"));
        assert!(!stderr.contains("provider-body-must-not-appear"));
        assert!(!stderr.contains("must-not-be-used"));
        assert!(server.is_running()?);
        assert_eq!(observer.config().await?.revision, 1);
        // Old sessions survive /new and disconnect; replay uses an independent Client.
        let (replay, mut events) = Client::connect(&endpoint).await?;
        assert!(replay.subscribe(old_session, 0).await? > 1);
        assert_eq!(
            events.recv().await.ok_or("missing old session")?.session_id,
            old_session
        );
        replay.close();
        observer.close();
        server.stop().await?;
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn idle_server_loss_exits_even_while_stdin_is_open() -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (directory, endpoint, mut server, listener, observer) = fixture(true).await?;
        let mut cli = Cli::spawn(&endpoint, directory.path())?;
        cli.prompt().await?;
        cli.send("hello\n").await?;
        let (_, stream) = request(&listener).await?;
        cli.session(&server).await?;
        respond(stream, 200, "done").await?;
        assert_eq!(cli.prompt().await?, "done\nmoly> ");
        server.stop().await?;
        assert!(
            cli.stdin.is_some(),
            "stdin must remain open through process exit"
        );
        assert!(!cli.child.wait().await?.success());
        assert!(
            cli.diagnostic()
                .await?
                .contains("Agent Server disconnected")
        );
        observer.close();
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn opencode_go_cli_preserves_session_headers_across_tool_errors_turns_and_new()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let (directory, endpoint, mut server, listener, observer) = fixture(false).await?;
        std::fs::write(directory.path().join("notes.txt"), "go-notes")?;
        let provider = provider_process::provider(json!({}))?;
        let url = format!(
            "http://{}/zen/go/v1/chat/completions",
            listener.local_addr()?
        );
        let mut cli = Cli::spawn_with_env(
            &endpoint,
            directory.path(),
            &[
                ("MOLY_PROVIDER", "opencode-go"),
                ("MOLY_PROVIDER_EXECUTABLE", &provider.command.executable),
                ("MOLY_MODEL_ENDPOINT", &url),
                ("MOLY_MODEL", "kimi-k2.6"),
                ("MOLY_API_KEY", "go-test-key"),
            ],
        )?;
        assert_eq!(cli.prompt().await?, "moly> ");
        assert_eq!(observer.config().await?.revision, 0);
        cli.send("read notes\n").await?;
        let (headers, body, stream) = http_request(&listener).await?;
        let session = cli.session(&server).await?;
        check_go_headers(&headers, session);
        assert_eq!(body["model"], "kimi-k2.6");
        let config = observer.config().await?.config.ok_or("missing config")?;
        assert_eq!(config.provider.options["profile"], "opencode-go");
        let tool_message = json!({
            "role": "assistant", "content": null, "reasoning_content": "opaque-go-reasoning",
            "tool_calls": [{"id": "go-read", "type": "function", "function": {
                "name": "read_file", "arguments": "{\"path\":\"missing-private-file\"}"
            }}]
        });
        respond_body(
            stream,
            200,
            json!({"choices": [{"message": tool_message, "finish_reason": "tool_calls"}]}),
        )
        .await?;
        let (headers, body, stream) = tokio::select! {
            request = http_request(&listener) => request?,
            prompt = cli.prompt() => {
                prompt?;
                return Err("run ended before Go tool-error continuation".into());
            },
        };
        check_go_headers(&headers, session);
        assert_eq!(body["messages"][1], tool_message);
        assert_eq!(body["messages"][2]["tool_call_id"], "go-read");
        let tool_error: Value = serde_json::from_str(
            body["messages"][2]["content"]
                .as_str()
                .ok_or("missing tool error")?,
        )?;
        assert_eq!(
            tool_error,
            json!({"error":{
                "code":"tool_file_not_found", "message":"Requested file was not found",
            }})
        );
        let corrected = json!({
            "role": "assistant", "content": null, "reasoning_content": "opaque-go-correction",
            "tool_calls": [{"id": "go-corrected", "type": "function", "function": {
                "name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"
            }}]
        });
        respond_body(
            stream,
            200,
            json!({"choices":[{
                "message":corrected, "finish_reason":"tool_calls",
            }]}),
        )
        .await?;
        let (headers, body, stream) = http_request(&listener).await?;
        check_go_headers(&headers, session);
        assert_eq!(body["messages"][1], tool_message);
        assert_eq!(body["messages"][3], corrected);
        assert_eq!(body["messages"][4]["tool_call_id"], "go-corrected");
        let tool_output: Value = serde_json::from_str(
            body["messages"][4]["content"]
                .as_str()
                .ok_or("missing tool output")?,
        )?;
        assert_eq!(tool_output, json!({"content":"go-notes"}));
        let answer =
            json!({"role": "assistant", "content": "done", "reasoning_content": "opaque-go-final"});
        respond_body(
            stream,
            200,
            json!({"choices": [{"message": answer, "finish_reason": "stop"}]}),
        )
        .await?;
        assert_eq!(cli.prompt().await?, "done\nmoly> ");
        cli.send("again\n").await?;
        let (headers, body, stream) = http_request(&listener).await?;
        check_go_headers(&headers, session);
        assert_eq!(body["messages"][5], answer);
        respond(stream, 200, "second").await?;
        assert_eq!(cli.prompt().await?, "second\nmoly> ");
        cli.send("/new\nnew\n").await?;
        assert_eq!(
            cli.prompt().await?,
            "New conversation on the next message.\nmoly> "
        );
        let (headers, body, stream) = http_request(&listener).await?;
        let new_session = cli.session(&server).await?;
        assert_ne!(new_session, session);
        check_go_headers(&headers, new_session);
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "new"}])
        );
        respond(stream, 200, "fresh").await?;
        assert_eq!(cli.prompt().await?, "fresh\nmoly> ");
        cli.send("/quit\n").await?;
        assert_eq!(cli.finish().await?, (String::new(), String::new()));
        assert!(server.is_running()?);
        observer.close();
        server.stop().await?;
        Ok::<_, TestError>(())
    })
    .await?
}

fn check_go_headers(headers: &str, session: SessionId) {
    let headers = headers.to_ascii_lowercase();
    assert!(headers.starts_with("post /zen/go/v1/chat/completions http/1.1\r\n"));
    assert!(headers.contains(&format!("x-opencode-session: {session}\r\n")));
    assert!(headers.contains(&format!(
        "user-agent: moly/{}\r\n",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(headers.contains("authorization: bearer go-test-key\r\n"));
}

#[path = "../support/provider_process.rs"]
mod provider_process;

async fn configure(
    client: &Client,
    listener: &TcpListener,
    directory: &std::path::Path,
) -> Result<(), TestError> {
    client
        .apply_config(
            0,
            ResolvedConfig {
                provider: provider_process::provider(json!({"model_endpoint":format!("http://{}/chat/completions", listener.local_addr()?), "model":"repl-mock"}))?,
                workspace: directory.to_string_lossy().into_owned(),
                secret_ref: None,
            },
        )
        .await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn non_unicode_settings_never_fall_back_and_configured_servers_still_win()
-> Result<(), TestError> {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStringExt;

    tokio::time::timeout(Duration::from_secs(20), async {
        for profile in ["openai", "opencode-go"] {
            for variable in [
                "MOLY_MODEL_ENDPOINT",
                "MOLY_MODEL",
                "MOLY_API_KEY",
                "MOLY_PROVIDER",
            ] {
                let (directory, endpoint, mut server, listener, observer) = fixture(false).await?;
                // If decoding regresses, a missing executable prevents any model
                // call, including to an accidentally selected public default URL.
                let missing_provider = directory.path().join("must-not-launch-provider");
                let url = format!("http://{}/chat/completions", listener.local_addr()?);
                let invalid = OsString::from_vec(b"private-config-sentinel-\xff".to_vec());
                let mut cli = Cli::spawn_with_env(
                    &endpoint,
                    directory.path(),
                    &[
                        ("MOLY_PROVIDER", OsStr::new(profile)),
                        ("MOLY_PROVIDER_EXECUTABLE", missing_provider.as_os_str()),
                        ("MOLY_MODEL_ENDPOINT", OsStr::new(&url)),
                        ("MOLY_MODEL", OsStr::new("repl-mock")),
                        ("MOLY_API_KEY", OsStr::new("test-key")),
                        (variable, invalid.as_os_str()),
                    ],
                )?;
                assert_eq!(cli.prompt().await?, "moly> ");
                cli.send("not submitted\n").await?;
                assert_eq!(cli.prompt().await?, "moly> ");
                let error = cli.diagnostic().await?;
                assert!(
                    error.contains("invalid_config"),
                    "{profile}/{variable}: {error}"
                );
                assert!(error.contains(variable));
                assert!(!error.contains("private-config-sentinel"));
                assert_eq!(observer.config().await?.revision, 0);

                configure(&observer, &listener, directory.path()).await?;
                cli.send("repaired\n").await?;
                let (body, stream) = request(&listener).await?;
                assert_eq!(
                    body["messages"],
                    json!([{"role": "user", "content": "repaired"}])
                );
                cli.session(&server).await?;
                respond(stream, 200, "working").await?;
                assert_eq!(cli.prompt().await?, "working\nmoly> ");
                cli.send("/quit\n").await?;
                assert_eq!(cli.finish().await?, (String::new(), String::new()));
                assert!(server.is_running()?);
                observer.close();
                server.stop().await?;
            }
        }
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn configuration_error_can_be_repaired_without_restarting_the_repl() -> Result<(), TestError>
{
    tokio::time::timeout(Duration::from_secs(10), async {
        let (directory, endpoint, mut server, listener, observer) = fixture(false).await?;
        let mut cli = Cli::spawn(&endpoint, directory.path())?;
        cli.prompt().await?;
        cli.send("not submitted\n").await?;
        assert_eq!(cli.prompt().await?, "moly> ");
        let error = cli.diagnostic().await?;
        assert!(error.contains("invalid_config"));
        assert!(!error.contains("must-not-be-used"));
        assert_eq!(observer.config().await?.revision, 0);
        configure(&observer, &listener, directory.path()).await?;
        cli.send("repaired\n").await?;
        let (body, stream) = request(&listener).await?;
        assert_eq!(
            body["messages"],
            json!([{"role":"user", "content":"repaired"}])
        );
        cli.session(&server).await?;
        respond(stream, 200, "working").await?;
        assert_eq!(cli.prompt().await?, "working\nmoly> ");
        cli.send("/quit\n").await?;
        assert_eq!(cli.finish().await?, (String::new(), String::new()));
        assert!(server.is_running()?);
        observer.close();
        server.stop().await?;
        Ok::<_, TestError>(())
    })
    .await?
}
