//! A launch audit needs the exact child PID and captured stderr, neither of which
//! the shared ServerProcess exposes. Lifecycle tests still use that shared fixture.
use crate::protocol::ServerId;
use serde_json::Value;
use std::{env, fs::File, io, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

type TestError = Box<dyn std::error::Error + Send + Sync>;

pub(super) struct AuditedServer {
    child: Child,
    pub(super) pid: u32,
    pub(super) server_id: ServerId,
}

impl AuditedServer {
    pub(super) async fn spawn(endpoint: &str, stderr: &Path) -> Result<Self, TestError> {
        let binary = match env::var_os("MOLY_TEST_SERVER_BIN") {
            Some(path) => path.into(),
            None => env::current_exe()?
                .parent()
                .and_then(|deps| deps.parent())
                .ok_or("test executable has no profile directory")?
                .join(format!("moly-server{}", env::consts::EXE_SUFFIX)),
        };
        if !binary.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Server not found at {}; build it first with `cargo build --locked --workspace --bins` or set MOLY_TEST_SERVER_BIN",
                    binary.display()
                ),
            )
            .into());
        }
        let mut command = Command::new(binary);
        command
            .args(["--endpoint", endpoint])
            .env_clear()
            .env("RUST_LOG", "debug")
            .env(
                "MOLY_PARENT_CANARY",
                "ambient-secret-must-not-reach-provider",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // A file cannot deadlock on an unread pipe; inspect it after reaping.
            .stderr(Stdio::from(File::create(stderr)?))
            .kill_on_drop(true);
        #[cfg(windows)]
        if let Some(root) = env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let mut child = command.spawn()?;
        let pid = child.id().ok_or("missing Server PID")?;
        let mut stdout = BufReader::new(child.stdout.take().ok_or("missing Server stdout")?);
        let mut line = String::new();
        if stdout.read_line(&mut line).await? == 0 {
            return Err("Server exited before readiness; inspect its captured stderr".into());
        }
        let ready: Value = serde_json::from_str(&line)?;
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["endpoint"], endpoint);
        let server_id = serde_json::from_value(ready["server_id"].clone())?;
        Ok(Self {
            child,
            pid,
            server_id,
        })
    }

    pub(super) async fn stop(&mut self) -> Result<(), TestError> {
        self.child.kill().await?;
        self.child.wait().await?;
        Ok(())
    }
}
