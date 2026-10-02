//! Cancelled connection establishment must not leave a hidden live peer.
use crate::client::{Client, Tool};
use crate::transport::{Inbox, Incoming, Peer, read_frame};
use moly_protocol::{ExecutorId, SERVER_VERSION, ServerId, SessionId, ToolDefinition};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, BufReader};

async fn request_id(inbox: &mut Inbox) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    match inbox.recv().await {
        Some(Incoming::Request { id, .. }) => Ok(id),
        _ => Err("expected request".into()),
    }
}
#[tokio::test]
async fn closing_during_registration_cannot_resurrect_captured_clients()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let (peer, mut inbox) = Peer::spawn(server_stream);
        let connecting = tokio::spawn(Client::from_stream(client_stream));
        let id = request_id(&mut inbox).await?;
        peer.respond(
            id,
            Ok(json!({"server_id":ServerId::new(),"role":"server","protocol_version":SERVER_VERSION})),
        )
        .await?;
        let (client, _events) = connecting.await??;
        let marker = Arc::new(());
        let weak = Arc::downgrade(&marker);
        let captured = client.clone();
        let tool = Tool::new(
            ToolDefinition {
                name: "probe".into(),
                description: "probe".into(),
                input_schema: json!({}),
            },
            move |_| {
                let _ = (&captured, &marker);
                async { Ok(Value::Null) }
            },
        );
        let session = SessionId::new();
        let initial_client = client.clone();
        let initial =
            tokio::spawn(async move { initial_client.register_tools(session, vec![tool]).await });
        let id = request_id(&mut inbox).await?;
        peer.respond(id, Ok(json!({"executor_id":ExecutorId::new()})))
            .await?;
        initial.await??;
        let replacing_client = client.clone();
        let replacing =
            tokio::spawn(async move { replacing_client.register_tools(session, vec![]).await });
        request_id(&mut inbox).await?;
        client.close();
        assert!(replacing.await?.is_err());
        drop(client);
        assert!(
            weak.upgrade().is_none(),
            "rollback must not recreate a callback ownership cycle after close"
        );
        peer.close();
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}
#[tokio::test]
async fn canceling_registration_closes_uncertain_connection_and_releases_callbacks()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let (peer, mut inbox) = Peer::spawn(server_stream);
        let connecting = tokio::spawn(Client::from_stream(client_stream));
        let id = request_id(&mut inbox).await?;
        peer.respond(
            id,
            Ok(json!({"server_id":ServerId::new(),"role":"server","protocol_version":SERVER_VERSION})),
        )
        .await?;
        let (client, mut events) = connecting.await??;
        let marker = Arc::new(());
        let weak = Arc::downgrade(&marker);
        let captured = client.clone();
        let tool = Tool::new(
            ToolDefinition {
                name: "probe".into(),
                description: "probe".into(),
                input_schema: json!({}),
            },
            move |_| {
                let _ = (&captured, &marker);
                async { Ok(Value::Null) }
            },
        );
        let registering = client.clone();
        let registration = tokio::spawn(async move {
            registering
                .register_tools(SessionId::new(), vec![tool])
                .await
        });
        request_id(&mut inbox).await?;
        registration.abort();
        assert!(registration.await.is_err());
        assert!(
            weak.upgrade().is_none(),
            "canceled registration must not retain callback cycles"
        );
        client.closed().await;
        assert!(events.recv().await.is_none());
        assert!(inbox.recv().await.is_none());
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}
#[tokio::test]
async fn aborting_handshake_closes_stream() -> Result<(), Box<dyn std::error::Error>> {
    let (client, server) = tokio::io::duplex(4096);
    let connecting = tokio::spawn(Client::from_stream(client));
    let mut server = BufReader::new(server);
    assert!(read_frame(&mut server).await?.is_some());
    connecting.abort();
    assert!(connecting.await.is_err());
    let mut byte = [0];
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        server.read(&mut byte),
    )
    .await;
    assert_eq!(
        read.expect("cancelled handshake must close its dispatcher")?,
        0
    );
    Ok(())
}
