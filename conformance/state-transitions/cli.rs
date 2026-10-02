//! REPL startup and local commands must not depend on a backend.

use std::{error::Error, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type TestError = Box<dyn Error + Send + Sync>;

async fn script(args: &[&str], input: &[u8]) -> Result<std::process::Output, TestError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let directory = tempfile::tempdir()?;
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_moly"));
        command
            .args(args)
            .env_clear()
            .env("MOLY_PROVIDER", "unknown-private-profile")
            .env("MOLY_MODEL_ENDPOINT", "not a URL")
            .env("MOLY_MODEL", "")
            .env("MOLY_API_KEY", "invalid\ncredential")
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().ok_or("missing CLI stdin")?;
        stdin.write_all(input).await?;
        drop(stdin);
        Ok(child.wait_with_output().await?)
    })
    .await?
}

#[tokio::test]
async fn help_and_local_commands_stay_lazy() -> Result<(), TestError> {
    let output = script(
        &["--connect", "missing-repl-server"],
        b"\n/help\n/new\n/unknown\n/exit\n",
    )
    .await?;
    assert!(output.status.success(), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("/help"));
    assert!(stdout.contains("/new"));
    assert!(stdout.contains("/quit"));
    assert_eq!(stdout.matches("moly> ").count(), 5);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("Unknown command"));
    assert!(!stderr.contains("Server"));
    assert!(!stderr.contains("credential"));
    Ok(())
}

#[tokio::test]
async fn connection_error_returns_to_prompt_without_losing_buffered_input() -> Result<(), TestError>
{
    let output = script(
        &["--connect", "missing-repl-server"],
        b"//literal\n/help\n/quit\n",
    )
    .await?;
    assert!(output.status.success(), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("/help"));
    assert_eq!(stdout.matches("moly> ").count(), 3);
    assert!(String::from_utf8(output.stderr)?.contains("error:"));
    Ok(())
}

#[tokio::test]
async fn help_flag_and_invalid_arguments_have_conventional_exit_statuses() -> Result<(), TestError>
{
    let output = script(&["--help"], b"").await?;
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)?.contains("/help"));
    assert!(output.stderr.is_empty());
    let output = script(&["--not-an-option"], b"").await?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("usage:"));
    Ok(())
}

#[tokio::test]
async fn eof_after_local_commands_exits_without_a_backend() -> Result<(), TestError> {
    let output = script(&["--connect", "missing-repl-server"], b"/help\n\n").await?;
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout)?.matches("moly> ").count(),
        3
    );
    assert!(output.stderr.is_empty());
    Ok(())
}

#[tokio::test]
async fn first_prompt_and_quit_do_not_connect_or_resolve_invalid_environment()
-> Result<(), Box<dyn Error + Send + Sync>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let directory = tempfile::tempdir()?;
        let missing = directory.path().join("missing-server.sock");
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_moly"));
        command
            .env_clear()
            .env("MOLY_PROVIDER", "unknown-private-profile")
            .env("MOLY_MODEL_ENDPOINT", "not a URL")
            .env("MOLY_MODEL", "")
            .env("MOLY_API_KEY", "invalid\ncredential")
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        // A regression can fail to connect, but cannot spawn an unmanaged Server.
        command.arg("--connect").arg(&missing);
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().ok_or("missing CLI stdin")?;
        let mut stdout = child.stdout.take().ok_or("missing CLI stdout")?;
        let mut prompt = [0; 6];
        // Keep stdin open: EOF must not be what permits this first flush.
        stdout.read_exact(&mut prompt).await?;
        assert_eq!(&prompt, b"moly> ");
        assert!(child.try_wait()?.is_none());
        stdin.write_all(b"/quit\n").await?;
        stdin.flush().await?;
        child.stdout = Some(stdout);
        let output = child.wait_with_output().await?;
        assert!(output.status.success(), "CLI failed: {:?}", output.stderr);
        assert!(output.stdout.is_empty(), "unexpected post-quit output");
        assert!(
            output.stderr.is_empty(),
            "backend was consulted: {:?}",
            output.stderr
        );
        drop(stdin);
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await?
}
