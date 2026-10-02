use crate::{client::Client, tests::server_process::ServerProcess};

#[tokio::test]
async fn standalone_process_does_not_read_agent_environment_or_exit_with_client()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        #[cfg(unix)]
        let directory = tempfile::tempdir()?;
        #[cfg(unix)]
        let endpoint = directory.path().join("s").to_string_lossy().into_owned();
        #[cfg(windows)]
        let endpoint = format!("moly-process-{}", moly_protocol::ConnectionId::new());
        let mut server = ServerProcess::spawn(&endpoint).await?;
        let (client, mut events) = Client::connect(&endpoint).await?;
        let identity = client.server_id();
        assert_eq!(identity, server.server_id);
        assert!(client.config().await?.config.is_none());
        let session = client.create_session().await?;
        client.close();
        assert!(events.recv().await.is_none());
        let (attached, mut replay) = Client::connect(&endpoint).await?;
        assert_eq!(attached.server_id(), identity);
        assert!(attached.config().await?.config.is_none());
        assert_eq!(attached.subscribe(session, 0).await?, 1);
        assert_eq!(
            replay
                .recv()
                .await
                .ok_or("session did not survive disconnection")?
                .session_id,
            session
        );
        assert!(server.is_running()?);
        attached.close();
        server.stop().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}
