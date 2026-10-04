//! Direct CLI process regressions use independent local Providers, never live services.
#[cfg(unix)]
use std::time::Duration;
use std::{
    error::Error,
    io::Write,
    process::{Command, Stdio},
};

type TestError = Box<dyn Error + Send + Sync>;

#[test]
fn direct_flag_and_local_commands_stay_lazy() -> Result<(), TestError> {
    let directory = tempfile::tempdir()?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_moly"));
    command
        .arg("--direct")
        .env_clear()
        .env("MOLY_PROVIDER", "unknown-private-profile")
        .env(
            "MOLY_PROVIDER_EXECUTABLE",
            directory.path().join("missing-provider"),
        )
        .env("MOLY_AUTH_STATE_FILE", directory.path().join("auth.json"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(b"\n/help\n/new\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "--direct rejected: {:?}",
        output.stderr
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert_eq!(stdout.matches("moly> ").count(), 4);
    assert!(stdout.contains("/help"));
    assert!(stdout.contains("Agent Server manages the agentic loop"));
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
    Ok(())
}

#[cfg(unix)]
async fn scenario(name: &str) -> Result<(), TestError> {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/direct.py");
    let output = tokio::time::timeout(
        Duration::from_secs(40),
        tokio::process::Command::new("python3")
            .arg(fixture)
            .arg(env!("CARGO_BIN_EXE_moly"))
            .arg(name)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "independent Python fixture {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn first_prompt_and_local_commands_never_launch_a_provider_or_server() -> Result<(), TestError>
{
    scenario("lazy").await
}

#[cfg(unix)]
#[tokio::test]
async fn auth_registration_is_private_and_credentials_are_cli_memory_only() -> Result<(), TestError>
{
    scenario("auth").await
}

#[cfg(unix)]
#[tokio::test]
async fn turns_preserve_metadata_and_ids_without_accepting_unadvertised_tools()
-> Result<(), TestError> {
    scenario("history").await
}

#[cfg(unix)]
#[tokio::test]
async fn ctrl_c_kills_model_children_and_keeps_rotation_but_discards_cancelled_context()
-> Result<(), TestError> {
    scenario("cancel-model").await
}

#[cfg(unix)]
#[tokio::test]
async fn queued_input_and_eof_wait_for_model_completion() -> Result<(), TestError> {
    scenario("queued-eof").await
}

#[cfg(unix)]
#[tokio::test]
async fn auth_ctrl_c_eof_and_quit_drop_children_without_rolling_back_credentials()
-> Result<(), TestError> {
    scenario("cancel-auth").await
}

#[cfg(unix)]
#[tokio::test]
async fn server_and_direct_failures_never_select_a_fallback() -> Result<(), TestError> {
    scenario("no-fallback").await
}
