//! CLI-owned connection, configuration, and unmanaged Server startup policy.
use crate::Error;
use moly_client::{
    Client, Events,
    protocol::{
        self, ResolvedConfig, SessionId,
        model::{ComponentCommand, ProviderConfig},
    },
};
use std::{future::Future, time::Duration};
use tokio::io::AsyncBufReadExt;

pub(crate) async fn rpc<T>(
    request: impl Future<Output = Result<T, moly_client::Error>>,
) -> Result<T, Error> {
    Ok(tokio::time::timeout(Duration::from_secs(10), request).await??)
}

pub(crate) struct Backend {
    pub(crate) client: Client,
    pub(crate) events: Events,
    session: Option<SessionId>,
    endpoint: String,
}

impl Backend {
    pub(crate) async fn connect(endpoint: &str) -> Result<Self, Error> {
        let (client, events) = rpc(Client::connect(endpoint)).await?;
        Ok(Self {
            client,
            events,
            session: None,
            endpoint: endpoint.into(),
        })
    }

    pub(crate) async fn session(&mut self) -> Result<SessionId, Error> {
        if let Some(session) = self.session {
            return Ok(session);
        }
        let current = rpc(self.client.config()).await?;
        // Do not even resolve local config when attaching to a configured authority.
        if current.config.is_none() {
            let secret = optional_env("MOLY_API_KEY")?;
            let config = ResolvedConfig {
                provider: provider_config()?,
                workspace: std::env::current_dir()?.to_string_lossy().into_owned(),
                secret_ref: secret.as_ref().map(|_| "cli-provider".into()),
            };
            rpc(self.client.validate_config(&config)).await?;
            if let Some(secret) = secret {
                rpc(self.client.put_secret("cli-provider", &secret)).await?;
            }
            rpc(self.client.apply_config(current.revision, config)).await?;
        }
        let session = rpc(self.client.create_session()).await?;
        rpc(self.client.subscribe(session, 0)).await?;
        self.session = Some(session);
        eprintln!(
            "Server {} at {}; session {session}",
            self.client.server_id(),
            self.endpoint
        );
        Ok(session)
    }

    pub(crate) async fn new_session(&mut self) -> Result<(), Error> {
        if let Some(session) = self.session {
            rpc(self.client.unsubscribe(session)).await?;
            self.session = None;
        }
        Ok(())
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.client.close();
    }
}

fn optional_env(name: &'static str) -> Result<Option<String>, Error> {
    // Invalid text is explicit configuration, not permission to pick a different
    // endpoint/model or omit authentication. Never retain its possibly secret value.
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(Error::InvalidEnvironment(name)),
    }
}

fn provider_config() -> Result<ProviderConfig, Error> {
    let profile = optional_env("MOLY_PROVIDER")?.unwrap_or_else(|| "openai".into());
    let options = provider_options(
        &profile,
        optional_env("MOLY_MODEL_ENDPOINT")?,
        optional_env("MOLY_MODEL")?,
    )?;
    let executable = match std::env::var_os("MOLY_PROVIDER_EXECUTABLE") {
        Some(path) => std::path::PathBuf::from(path),
        None => std::env::current_exe()?.with_file_name(format!(
            "moly-provider-openai{}",
            std::env::consts::EXE_SUFFIX
        )),
    };
    // Resolve in the Client, not in Server or SDK. No shell/PATH interpretation.
    let executable = if executable.is_absolute() {
        executable
    } else {
        std::env::current_dir()?.join(executable)
    };
    let mut env = std::collections::BTreeMap::new();
    env.insert("RUST_LOG".into(), "off".into());
    #[cfg(windows)]
    if let Ok(root) = std::env::var("SystemRoot") {
        env.insert("SystemRoot".into(), root);
    }
    Ok(ProviderConfig {
        command: ComponentCommand {
            executable: executable.to_string_lossy().into_owned(),
            args: vec![],
            env,
        },
        options,
    })
}

fn provider_options(
    profile: &str,
    endpoint: Option<String>,
    model: Option<String>,
) -> Result<serde_json::Value, Error> {
    let (default_endpoint, default_model) = match profile {
        "openai" => ("https://api.openai.com/v1/chat/completions", "gpt-4.1-mini"),
        "opencode-go" => (
            "https://opencode.ai/zen/go/v1/chat/completions",
            "kimi-k2.6",
        ),
        _ => return Err(Error::ProviderProfile),
    };
    let mut options = serde_json::json!({
        "model_endpoint": endpoint.unwrap_or_else(|| default_endpoint.into()),
        "model": model.unwrap_or_else(|| default_model.into()),
    });
    if profile == "opencode-go" {
        options["profile"] = serde_json::json!(profile);
    }
    Ok(options)
}

pub(crate) async fn spawn_server() -> Result<String, Error> {
    let identity = protocol::ConnectionId::new();
    #[cfg(unix)]
    let endpoint = {
        use std::os::unix::fs::DirBuilderExt;
        let directory = std::env::temp_dir().join(format!("moly-{identity}"));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        directory.join("server.sock").to_string_lossy().into_owned()
    };
    #[cfg(windows)]
    let endpoint = format!("moly-{identity}");
    let binary = std::env::current_exe()?
        .with_file_name(format!("moly-server{}", std::env::consts::EXE_SUFFIX));
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["--endpoint", &endpoint])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(false);
    // Terminal Ctrl-C must cancel the run through RPC, not kill its Server.
    #[cfg(unix)]
    command.process_group(0);
    // CREATE_NEW_PROCESS_GROUP disables inherited console Ctrl-C delivery.
    // https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
    #[cfg(windows)]
    command.creation_flags(0x0000_0200);
    let mut child = command.spawn().map_err(|error| {
        #[cfg(unix)]
        if let Some(directory) = std::path::Path::new(&endpoint).parent() {
            let _ = std::fs::remove_dir(directory);
        }
        Error::Spawn(error)
    })?;
    let output = child.stdout.take().expect("stdout was configured as piped");
    let mut reader = tokio::io::BufReader::new(output).lines();
    let ready = tokio::time::timeout(Duration::from_secs(10), reader.next_line()).await;
    let valid = matches!(&ready, Ok(Ok(Some(line))) if serde_json::from_str::<serde_json::Value>(line).ok()
        .is_some_and(|value| value.get("ready") == Some(&serde_json::Value::Bool(true))));
    if !valid {
        child.kill().await?;
        return Err(Error::Readiness);
    }
    // Diagnostic only: protocol identity/discovery never relies on this PID.
    if let Some(pid) = child.id() {
        eprintln!("Spawned unmanaged Server pid={pid}");
    }
    // Explicitly unmanaged: closing the CLI does not terminate the Server.
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_profiles_resolve_defaults_without_discovery() -> Result<(), Error> {
        assert_eq!(
            provider_options("openai", None, None)?,
            json!({
                "model_endpoint": "https://api.openai.com/v1/chat/completions", "model": "gpt-4.1-mini"
            })
        );
        assert_eq!(
            provider_options("opencode-go", None, None)?,
            json!({
                "profile": "opencode-go", "model_endpoint": "https://opencode.ai/zen/go/v1/chat/completions", "model": "kimi-k2.6"
            })
        );
        Ok(())
    }

    #[test]
    fn provider_profile_overrides_are_literal_and_unknown_profiles_never_fall_back()
    -> Result<(), Error> {
        let options = provider_options(
            "opencode-go",
            Some("http://localhost/exact".into()),
            Some("other-model".into()),
        )?;
        assert_eq!(options["model_endpoint"], "http://localhost/exact");
        assert_eq!(options["model"], "other-model");
        // Empty explicit values must reach validation, not select defaults.
        assert_eq!(
            provider_options("opencode-go", Some(String::new()), Some(String::new()))?["model"],
            ""
        );
        for profile in ["", "unknown-private-profile", "OPENCODE-GO"] {
            let error =
                provider_options(profile, None, None).expect_err("unknown profile must fail");
            assert!(matches!(error, Error::ProviderProfile));
            assert!(!error.to_string().contains("private"));
        }
        Ok(())
    }
}
