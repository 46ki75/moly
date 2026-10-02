//! Configuration remains data and validation is authoritative at the Server.
use crate::core::{Connection, Server};
use crate::tests::provider_process;
use moly_protocol::{
    ConfigApply, ConfigSnapshot, ResolvedConfig, SessionRef, ToolDefinition, ToolsRegister,
};
use serde_json::{Value, json};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn config(model: &str) -> Result<ResolvedConfig> {
    Ok(ResolvedConfig {
        provider: provider_process::provider(
            json!({"model_endpoint":"https://example.invalid/chat/completions", "model":model}),
        )?,
        workspace: std::env::temp_dir().to_string_lossy().into_owned(),
        secret_ref: None,
    })
}
#[tokio::test]
async fn invalid_config_never_advances_revision() -> Result {
    let server = Server::new()?;
    let (connection, _output) = Connection::new();
    for invalid in [config("")?, config("   ")?] {
        let result = server
            .request(
                &connection,
                "config.apply",
                serde_json::to_value(ConfigApply {
                    base_revision: 0,
                    config: invalid,
                })?,
            )
            .await;
        assert_eq!(
            result.expect_err("invalid model must fail validation").code,
            "invalid_config"
        );
        let snapshot: ConfigSnapshot = serde_json::from_value(
            server
                .request(&connection, "config.get", Value::Null)
                .await?,
        )?;
        assert_eq!(snapshot.revision, 0);
        assert!(snapshot.config.is_none());
    }
    Ok(())
}
#[tokio::test]
async fn stale_revision_does_not_launch_its_selected_provider() -> Result {
    let server = Server::new()?;
    let (connection, _output) = Connection::new();
    let valid = config("test-model")?;
    server
        .request(
            &connection,
            "config.apply",
            serde_json::to_value(ConfigApply {
                base_revision: 0,
                config: valid.clone(),
            })?,
        )
        .await?;
    let directory = tempfile::tempdir()?;
    let mut stale = valid;
    stale.provider.command.executable = directory
        .path()
        .join("must-not-be-launched")
        .to_string_lossy()
        .into_owned();
    let error = server
        .request(
            &connection,
            "config.apply",
            serde_json::to_value(ConfigApply {
                base_revision: 0,
                config: stale,
            })?,
        )
        .await
        .expect_err("stale configuration must be rejected before launch");
    assert_eq!(error.code, "revision_conflict");
    Ok(())
}

#[tokio::test]
async fn core_without_local_executor_accepts_client_hosted_file_capability() -> Result {
    let server = Server::with_local_executor(None)?;
    let (connection, _output) = Connection::new();
    let session: SessionRef = serde_json::from_value(
        server
            .request(&connection, "session.create", Value::Null)
            .await?,
    )?;
    let tools = ToolsRegister {
        session_id: session.session_id,
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "Client file API".into(),
            input_schema: json!({"type":"object"}),
        }],
    };
    let registered = server
        .request(&connection, "tools.register", serde_json::to_value(tools)?)
        .await?;
    assert!(registered.get("executor_id").is_some());
    Ok(())
}
#[tokio::test]
async fn fresh_server_has_no_config_and_rejects_unknown_commands() -> Result {
    let server = Server::new()?;
    let (connection, _output) = Connection::new();
    let snapshot: ConfigSnapshot = serde_json::from_value(
        server
            .request(&connection, "config.get", Value::Null)
            .await?,
    )?;
    assert_eq!(snapshot.revision, 0);
    assert!(snapshot.config.is_none());
    let error = server
        .request(&connection, "not.a.method", json!({}))
        .await
        .expect_err("unknown methods return structured errors");
    assert_eq!(error.code, "unknown_method");
    Ok(())
}
