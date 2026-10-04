//! CLI-owned connection, configuration, and unmanaged Agent Server startup policy.
use crate::{Error, auth_state::AuthState};
use moly_client::{
    Client, Events,
    protocol::{
        self, ConfigSnapshot, ResolvedConfig, SessionId,
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
    auth_state: Option<AuthState>,
}

impl Backend {
    pub(crate) async fn connect(endpoint: &str) -> Result<Self, Error> {
        let (client, events) = rpc(Client::connect(endpoint)).await?;
        Ok(Self {
            client,
            events,
            session: None,
            endpoint: endpoint.into(),
            auth_state: None,
        })
    }

    pub(crate) async fn session(&mut self) -> Result<SessionId, Error> {
        if let Some(session) = self.session {
            return Ok(session);
        }
        self.ensure_config().await?;
        let session = rpc(self.client.create_session()).await?;
        rpc(self.client.subscribe(session, 0)).await?;
        self.session = Some(session);
        eprintln!(
            "Agent Server {} at {}; session {session}",
            self.client.server_id(),
            self.endpoint
        );
        Ok(session)
    }

    pub(crate) async fn ensure_config(&mut self) -> Result<ConfigSnapshot, Error> {
        let current = rpc(self.client.config()).await?;
        // A configured authority wins: never overwrite it from ambient CLI config.
        if current.config.is_some() {
            return Ok(current);
        }
        let resolved = resolve_provider()?;
        let config = ResolvedConfig {
            provider: resolved.config,
            workspace: std::env::current_dir()?.to_string_lossy().into_owned(),
            secret_ref: resolved.credential_scope.then(|| "cli-provider".into()),
        };
        rpc(self.client.validate_config(&config)).await?;
        if let Some(secret) = resolved.credential {
            rpc(self.client.put_secret("cli-provider", &secret)).await?;
        }
        let snapshot = rpc(self.client.apply_config(current.revision, config)).await?;
        self.auth_state = resolved.auth_state;
        Ok(snapshot)
    }

    /// Determine local ownership before auth; never adopt arbitrary Provider data.
    pub(crate) fn owns_auth_state(&mut self, snapshot: &ConfigSnapshot) -> Result<bool, Error> {
        // Invalid/unselected ambient policy cannot grant local persistence or
        // preempt authentication using an already configured authority.
        if std::env::var("MOLY_PROVIDER").ok().as_deref() != Some("openai-codex") {
            return Ok(false);
        }
        let Some(config) = &snapshot.config else {
            return Ok(false);
        };
        if config.secret_ref.as_deref() != Some("cli-provider")
            || config.provider.command != provider_command("openai-codex")?
        {
            return Ok(false);
        }
        let Some(model) = std::env::var("MOLY_MODEL")
            .ok()
            .filter(|model| !model.trim().is_empty())
        else {
            return Ok(false);
        };
        if config
            .provider
            .options
            .get("model")
            .and_then(serde_json::Value::as_str)
            != Some(&model)
        {
            return Ok(false);
        }
        if self.auth_state.is_none() {
            if std::env::var_os("MOLY_AUTH_STATE_FILE").is_none() {
                return Ok(false);
            }
            self.auth_state = match AuthState::resolve(false) {
                Ok(state) => Some(state),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
        }
        let state = self.auth_state.as_ref().expect("state resolved");
        let intended = provider_config("openai-codex", Some(state))?;
        Ok(matches_profile(config, &intended))
    }

    pub(crate) async fn persist_registration(
        &mut self,
        snapshot: &ConfigSnapshot,
        registration: Option<serde_json::Value>,
        owned: bool,
    ) -> Result<(), Error> {
        if !owned {
            return Ok(());
        }
        let Some(registration) = registration else {
            return Ok(());
        };
        self.auth_state
            .as_mut()
            .expect("owned state resolved")
            .persist(registration.clone())?;
        // Persist first: status can recover a lost login result. A concurrent config
        // change must not be overwritten, even when it selects this same Provider.
        let current = rpc(self.client.config()).await?;
        if current.revision != snapshot.revision || current.config != snapshot.config {
            return Err(Error::AuthState(
                "registration saved locally; Agent Server config changed; not overwritten",
            ));
        }
        let mut config = current
            .config
            .ok_or(Error::AuthState("Agent Server is not configured"))?;
        if config.provider.options.get("registration") == Some(&registration) {
            return Ok(());
        }
        config.provider.options["registration"] = registration;
        rpc(self.client.apply_config(current.revision, config)).await?;
        Ok(())
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

/// CLI discovery shared by Server setup and the opt-in direct host. Credentials
/// stay out of Provider options, command arguments, and the explicit child env.
pub(crate) struct ResolvedProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) credential: Option<String>,
    pub(crate) credential_scope: bool,
    pub(crate) auth_state: Option<AuthState>,
}

pub(crate) fn resolve_provider() -> Result<ResolvedProvider, Error> {
    let profile = optional_env("MOLY_PROVIDER")?.unwrap_or_else(|| "openai".into());
    let auth_state = if profile == "openai-codex" {
        Some(AuthState::resolve(true)?)
    } else {
        None
    };
    // OAuth deliberately never even decodes MOLY_API_KEY, let alone sends it.
    let credential = if profile == "openai-codex" {
        None
    } else {
        optional_env("MOLY_API_KEY")?
    };
    Ok(ResolvedProvider {
        config: provider_config(&profile, auth_state.as_ref())?,
        credential_scope: secret_reference(&profile, credential.is_some()).is_some(),
        credential,
        auth_state,
    })
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

fn secret_reference(profile: &str, api_key: bool) -> Option<String> {
    (profile == "openai-codex" || api_key).then(|| "cli-provider".into())
}
fn matches_profile(config: &ResolvedConfig, intended: &ProviderConfig) -> bool {
    if let (Some(current), Some(saved)) = (
        config.provider.options.get("registration"),
        intended.options.get("registration"),
    ) && current != saved
    {
        return false;
    }
    let mut current_options = config.provider.options.clone();
    let mut intended_options = intended.options.clone();
    if let Some(options) = current_options.as_object_mut() {
        options.remove("registration");
    }
    if let Some(options) = intended_options.as_object_mut() {
        options.remove("registration");
    }
    config.secret_ref.as_deref() == Some("cli-provider")
        && config.provider.command == intended.command
        && current_options == intended_options
}
fn provider_binary(profile: &str) -> &'static str {
    if profile == "openai-codex" {
        "moly-provider-openai-codex"
    } else {
        "moly-provider-openai"
    }
}
fn codex_options(model: Option<String>, state: &AuthState) -> Result<serde_json::Value, Error> {
    let model = model
        .filter(|model| !model.trim().is_empty())
        .ok_or(Error::InvalidConfig(
            "MOLY_MODEL must be explicit and nonblank for openai-codex",
        ))?;
    let mut options = serde_json::json!({"model":model,"host_id":state.host_id});
    if let Some(registration) = &state.registration {
        options["registration"] = registration.clone();
    }
    Ok(options)
}
fn provider_config(profile: &str, state: Option<&AuthState>) -> Result<ProviderConfig, Error> {
    let options = if profile == "openai-codex" {
        codex_options(
            optional_env("MOLY_MODEL")?,
            state.ok_or(Error::AuthState("local auth state required"))?,
        )?
    } else {
        provider_options(
            profile,
            optional_env("MOLY_MODEL_ENDPOINT")?,
            optional_env("MOLY_MODEL")?,
        )?
    };
    Ok(ProviderConfig {
        command: provider_command(profile)?,
        options,
    })
}
fn provider_command(profile: &str) -> Result<ComponentCommand, Error> {
    let executable = match std::env::var_os("MOLY_PROVIDER_EXECUTABLE") {
        Some(path) => std::path::PathBuf::from(path),
        None => std::env::current_exe()?.with_file_name(format!(
            "{}{}",
            provider_binary(profile),
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
    Ok(ComponentCommand {
        executable: executable.to_string_lossy().into_owned(),
        args: vec![],
        env,
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
        eprintln!("Spawned unmanaged Agent Server pid={pid}");
    }
    // Explicitly unmanaged: closing the CLI does not terminate the Server.
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn codex_requires_explicit_model_identity_and_oauth_secret_scope() -> Result<(), Error> {
        let directory = tempfile::tempdir()?;
        let mut state = AuthState::load(directory.path().join("state"), true)?;
        for model in [None, Some(String::new()), Some("   ".into())] {
            assert!(matches!(
                codex_options(model, &state),
                Err(Error::InvalidConfig(_))
            ));
        }
        let options = codex_options(Some("chosen-model".into()), &state)?;
        assert_eq!(
            options,
            json!({"model":"chosen-model","host_id":state.host_id})
        );
        assert_eq!(
            provider_binary("openai-codex"),
            "moly-provider-openai-codex"
        );
        assert_eq!(provider_binary("opencode-go"), "moly-provider-openai");
        assert_eq!(
            secret_reference("openai-codex", false).as_deref(),
            Some("cli-provider")
        );
        assert_eq!(secret_reference("openai", false), None);
        state.persist(json!({"client_id":"issued","host_id":state.host_id}))?;
        assert_eq!(
            codex_options(Some("chosen-model".into()), &state)?["registration"],
            state.registration.expect("persisted")
        );
        Ok(())
    }
    #[test]
    fn a_different_saved_registration_is_not_adopted_from_the_server() {
        let intended = ProviderConfig {
            command: ComponentCommand {
                executable: "/explicit/provider".into(),
                args: vec![],
                env: Default::default(),
            },
            options: json!({"model":"chosen","host_id":"same-host","registration":{"client_id":"saved-account"}}),
        };
        let mut config = ResolvedConfig {
            provider: intended.clone(),
            workspace: "/workspace".into(),
            secret_ref: Some("cli-provider".into()),
        };
        assert!(matches_profile(&config, &intended));
        config.provider.options["registration"] = json!({"client_id":"another-account"});
        assert!(
            !matches_profile(&config, &intended),
            "two different nonempty registrations require explicit reconciliation"
        );
    }

    #[test]
    fn registration_updates_require_matching_local_profile_not_just_a_provider_name() {
        let intended = ProviderConfig {
            command: ComponentCommand {
                executable: "/explicit/moly-provider-openai-codex".into(),
                args: vec![],
                env: Default::default(),
            },
            options: json!({"model":"chosen","host_id":"local-host"}),
        };
        let mut config = ResolvedConfig {
            provider: intended.clone(),
            workspace: "/workspace".into(),
            secret_ref: Some("cli-provider".into()),
        };
        config.provider.options["registration"] = json!({"client_id":"recovered"});
        assert!(matches_profile(&config, &intended));
        for key in ["host_id", "model"] {
            let mut other = config.clone();
            other.provider.options[key] = json!("other");
            assert!(!matches_profile(&other, &intended));
        }
        let mut other = config.clone();
        other.provider.command.executable = "/other/provider".into();
        assert!(!matches_profile(&other, &intended));
        let mut other = config.clone();
        other.secret_ref = Some("other-client-scope".into());
        assert!(!matches_profile(&other, &intended));
        config.provider.options["unexpected"] = json!(true);
        assert!(!matches_profile(&config, &intended));
    }
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
