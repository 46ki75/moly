//! Exercise the public SDK without access to its Client or transport internals.
use crate::server_process::ServerProcess;
#[path = "../support/provider_process.rs"]
mod provider_process;
use moly_client::{
    Client, Error, Tool,
    protocol::{
        Body, ConfigSnapshot, EventKind, Message, ResolvedConfig, SERVER_VERSION as VERSION,
        ServerId, ToolDefinition,
    },
};
use serde_json::{Value, json};
use std::{io, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

type TestError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::test]
async fn public_client_config_tools_replay_and_clone_lifetime() -> Result<(), TestError> {
    fn shared_client<T: Clone + Send + Sync>() {}
    shared_client::<Client>();
    tokio::time::timeout(Duration::from_secs(10), async {
        let directory = tempfile::tempdir()?;
        #[cfg(unix)]
        let endpoint = directory.path().join("s").to_string_lossy().into_owned();
        #[cfg(windows)]
        let endpoint = format!("moly-sdk-{}", moly_client::protocol::ConnectionId::new());
        let mut server = ServerProcess::spawn(&endpoint).await?;
        let (client, mut events) = Client::connect(&endpoint).await?;
        assert_eq!(client.server_id(), server.server_id);
        let snapshot = client.config().await?;
        assert_eq!(snapshot.revision, 0);
        assert!(snapshot.config.is_none());
        let config = ResolvedConfig {
            provider: provider_process::provider(json!({"model_endpoint":"http://127.0.0.1:1/v1/chat/completions", "model":"sdk-contract-test"}))?,
            workspace: directory.path().to_string_lossy().into_owned(),
            secret_ref: Some("test-key".into()),
        };
        let mut invalid = config.clone();
        invalid.provider.options["model"] = json!("   ");
        assert!(matches!(client.validate_config(&invalid).await,
            Err(Error::Remote(error)) if error.code == "invalid_config"));
        client.validate_config(&config).await?;
        assert_eq!(
            client.config().await?.revision,
            0,
            "validation must not apply"
        );
        client
            .put_secret("test-key", "fake-local-test-secret")
            .await?;
        assert_eq!(client.apply_config(0, config.clone()).await?.revision, 1);
        assert!(matches!(client.apply_config(0, config.clone()).await,
            Err(Error::Remote(error)) if error.code == "revision_conflict"));
        let raw: ConfigSnapshot = client.request("config.get", ()).await?;
        assert_eq!(raw.config, Some(config));
        assert!(matches!(client.request::<Value>("sdk.unknown", ()).await,
            Err(Error::Remote(error)) if error.code == "unknown_method"));
        assert!(matches!(
            client.request::<u64>("config.get", ()).await,
            Err(Error::Json(_))
        ));

        let session = client.create_session().await?;
        let tool = Tool::new(
            ToolDefinition {
                name: "sdk_echo".into(),
                description: "Public async callback contract".into(),
                input_schema: json!({"type":"object"}),
            },
            |arguments| async move { Ok(arguments) },
        );
        client.register_tools(session, vec![tool]).await?;
        client.register_tools(session, vec![]).await?;
        assert_eq!(client.subscribe(session, 0).await?, 1);
        let created = events.recv().await.ok_or("missing creation event")?;
        assert_eq!(created.session_id, session);
        assert_eq!(created.seq, 1);
        assert!(matches!(created.kind, EventKind::SessionCreated));
        client.unsubscribe(session).await?;
        assert_eq!(client.subscribe(session, 0).await?, 1);
        assert_eq!(events.recv().await.ok_or("missing replay")?.seq, 1);

        let remaining = client.clone();
        drop(client);
        assert_eq!(
            remaining.config().await?.revision,
            1,
            "one clone must keep the connection alive"
        );
        let closed_clone = remaining.clone();
        remaining.close();
        closed_clone.closed().await;
        assert!(matches!(closed_clone.config().await, Err(Error::Closed)));
        assert!(matches!(
            closed_clone.register_tools(session, vec![]).await,
            Err(Error::Closed)
        ));
        assert!(events.recv().await.is_none());
        assert!(server.is_running()?);
        let (observer, mut replay) = Client::connect(&endpoint).await?;
        assert_eq!(observer.server_id(), server.server_id);
        assert_eq!(observer.config().await?.revision, 1);
        assert_eq!(observer.subscribe(session, 0).await?, 1);
        assert_eq!(
            replay
                .recv()
                .await
                .ok_or("session must survive Client closure")?
                .seq,
            1
        );
        server.stop().await?;
        observer.closed().await;
        assert!(replay.recv().await.is_none());
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn invalid_endpoint_has_a_public_io_error() -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        for endpoint in ["", "invalid\0endpoint"] {
            assert!(matches!(Client::connect(endpoint).await,
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::InvalidInput));
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn public_stream_handshake_rejects_incompatible_peers_and_closes_them()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        for (role, version) in [("daemon", VERSION), ("server", VERSION + 1)] {
            let (stream, peer) = tokio::io::duplex(4096);
            let connecting = tokio::spawn(Client::from_stream(stream));
            let mut peer = BufReader::new(peer);
            let mut line = String::new();
            peer.read_line(&mut line).await?;
            let request: Message = serde_json::from_str(&line)?;
            let id = match request.body {
                Body::Request { id, method, params } => {
                    assert_eq!(method, "initialize");
                    assert_eq!(params["protocol_version"], VERSION);
                    id
                }
                _ => return Err("expected initialization request".into()),
            };
            let response = Message::new(Body::Response {
                id,
                result: json!({"server_id":ServerId::new(), "role":role, "protocol_version":version}),
            });
            let mut frame = serde_json::to_vec(&response)?;
            frame.push(b'\n');
            peer.get_mut().write_all(&frame).await?;
            assert!(matches!(connecting.await?, Err(Error::Handshake)));
            assert_eq!(peer.read(&mut [0]).await?, 0, "rejected handshake must release its stream");
        }
        Ok::<_, TestError>(())
    }).await?
}
