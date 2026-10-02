//! Child-process fixture shared by Client-side Server conformance tests.

use crate::protocol::ServerId;
use serde::Deserialize;
use std::{env, error::Error, io, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

type TestError = Box<dyn Error + Send + Sync>;

fn binary() -> io::Result<PathBuf> {
    let path = match env::var_os("MOLY_TEST_SERVER_BIN") {
        Some(path) => PathBuf::from(path),
        None => env::current_exe()?
            .parent()
            .and_then(|deps| deps.parent())
            .ok_or_else(|| io::Error::other("test executable has no profile directory"))?
            .join(format!("moly-server{}", env::consts::EXE_SUFFIX)),
    };
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "moly-server binary not found at {}; run `cargo build --locked --workspace --bins` before running tests, or set MOLY_TEST_SERVER_BIN to the built binary",
                path.display()
            ),
        ));
    }
    Ok(path)
}

#[derive(Deserialize)]
struct Readiness {
    ready: bool,
    endpoint: String,
    server_id: ServerId,
}

/// Owns a real Server process, killing it if fixture setup or a test fails.
pub(crate) struct ServerProcess {
    child: Child,
    /// Identity announced by the child, to compare with successful initialization.
    pub(crate) server_id: ServerId,
}

impl ServerProcess {
    /// Starts a prebuilt Server and awaits its readiness record without polling.
    pub(crate) async fn spawn(endpoint: &str) -> Result<Self, TestError> {
        let path = binary()?;
        let mut command = Command::new(&path);
        command
            .args(["--endpoint", endpoint])
            .env_clear()
            // Configuration must arrive through the Client protocol, never ambient discovery.
            .env("MOLY_MODEL_ENDPOINT", "invalid-and-must-not-be-read")
            .env("MOLY_MODEL", "")
            .env("RUST_LOG", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Preserve startup diagnostics without an unread pipe that could fill.
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(windows)]
        if let Some(root) = env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let mut child = command.spawn().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to start moly-server at {}: {error}", path.display()),
            )
        })?;
        let stdout = child.stdout.take().ok_or("missing Server readiness pipe")?;
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "moly-server did not report readiness within 5 seconds",
                )
            })??
            .ok_or("moly-server closed stdout before readiness")?;
        let readiness: Readiness = serde_json::from_str(&line)?;
        assert!(readiness.ready, "Server did not announce readiness");
        assert_eq!(readiness.endpoint, endpoint);
        Ok(Self {
            child,
            server_id: readiness.server_id,
        })
    }

    /// Checks once that the Server has not exited; this is not a readiness poll.
    pub(crate) fn is_running(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    /// Explicitly kills and reaps the child on the successful fixture path.
    pub(crate) async fn stop(&mut self) -> io::Result<()> {
        self.child.kill().await?;
        self.child.wait().await?;
        Ok(())
    }
}
