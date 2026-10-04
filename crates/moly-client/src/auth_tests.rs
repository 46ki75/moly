//! Hermetic auth callback tests over a fake full-duplex Server stream.
use crate::{
    Client, Error, Events, Interaction,
    transport::{Inbox, Incoming, Peer},
};
use moly_protocol::{AuthAttemptId, SERVER_VERSION, ServerId, auth::*};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

type TestError = Box<dyn std::error::Error + Send + Sync>;
async fn setup() -> Result<(Client, Events, Peer, Inbox), TestError> {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let (peer, mut inbox) = Peer::spawn(server_stream);
    let connecting = tokio::spawn(Client::from_stream(client_stream));
    let (id, method, _) = request(&mut inbox).await?;
    assert_eq!(method, "initialize");
    peer.respond(
        id,
        Ok(json!({"server_id":ServerId::new(),"role":"server","protocol_version":SERVER_VERSION})),
    )
    .await?;
    let (client, events) = connecting.await??;
    Ok((client, events, peer, inbox))
}
async fn request(inbox: &mut Inbox) -> Result<(u64, String, Value), TestError> {
    match inbox.recv().await {
        Some(Incoming::Request { id, method, params }) => Ok((id, method, params)),
        _ => Err("expected request".into()),
    }
}
fn command(operation: AuthOperation) -> AuthCommand {
    AuthCommand {
        attempt_id: AuthAttemptId::new(),
        operation,
        config_revision: 7,
    }
}
fn status(attempt_id: AuthAttemptId) -> Value {
    json!({"attempt_id":attempt_id,"authenticated":true,"registration":null,"revocation_confirmed":null})
}
async fn interaction(
    peer: &Peer,
    attempt_id: AuthAttemptId,
) -> Result<Value, crate::transport::Error> {
    peer.request(
        "interaction.request",
        json!({"attempt_id":attempt_id,"url":"https://example.test/authorize"}),
    )
    .await
}
fn unavailable(result: Result<Value, crate::transport::Error>) {
    assert!(
        matches!(result, Err(crate::transport::Error::Remote(error)) if error.code == "interaction_unavailable")
    );
}

#[tokio::test]
async fn interaction_is_attempt_scoped_installed_before_rpc_and_allows_nested_commands()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, _events, peer, mut inbox) = setup().await?;
        let auth = command(AuthOperation::Login);
        let attempt_id = auth.attempt_id;
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let nested = client.clone();
        let captured = Arc::new(());
        let weak = Arc::downgrade(&captured);
        let handler = Interaction::new(move |request| {
            assert_eq!(request.attempt_id, attempt_id);
            let nested = nested.clone();
            let counted = counted.clone();
            let captured = captured.clone();
            async move {
                let _ = captured;
                counted.fetch_add(1, Ordering::SeqCst);
                nested.config().await.map_err(|_| {
                    moly_protocol::ProtocolError::new("test", "nested command failed")
                })?;
                Ok(InteractionOutcome::Opened)
            }
        });
        let authenticating = client.clone();
        let task =
            tokio::spawn(async move { authenticating.authenticate(auth, Some(handler)).await });
        let (id, method, params) = request(&mut inbox).await?;
        assert_eq!(method, "provider.auth");
        assert_eq!(params["config_revision"], 7);
        unavailable(interaction(&peer, AuthAttemptId::new()).await);
        let reverse = peer.clone();
        let callback = tokio::spawn(async move { interaction(&reverse, attempt_id).await });
        let (nested_id, method, _) = request(&mut inbox).await?;
        assert_eq!(method, "config.get");
        peer.respond(nested_id, Ok(json!({"revision":7,"config":null})))
            .await?;
        let response: InteractionResponse = serde_json::from_value(callback.await??)?;
        assert_eq!(response.attempt_id, attempt_id);
        assert_eq!(response.outcome, InteractionOutcome::Opened);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        peer.respond(id, Ok(status(attempt_id))).await?;
        assert!(task.await??.authenticated);
        assert!(
            weak.upgrade().is_none(),
            "no captured Client cycle after completion"
        );
        unavailable(interaction(&peer, attempt_id).await);
        client.close();
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn default_headless_and_non_login_attempts_never_present() -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, _events, peer, mut inbox) = setup().await?;
        for operation in [
            AuthOperation::Login,
            AuthOperation::Status,
            AuthOperation::Logout,
        ] {
            let auth = command(operation);
            let attempt_id = auth.attempt_id;
            let handler = (operation != AuthOperation::Login).then(|| {
                Interaction::new(|_| async {
                    panic!("status/logout must not present");
                    #[allow(unreachable_code)]
                    Ok(InteractionOutcome::Opened)
                })
            });
            let authenticating = client.clone();
            let task =
                tokio::spawn(async move { authenticating.authenticate(auth, handler).await });
            let (id, _, _) = request(&mut inbox).await?;
            unavailable(interaction(&peer, attempt_id).await);
            peer.respond(id, Ok(status(attempt_id))).await?;
            task.await??;
        }
        unavailable(interaction(&peer, AuthAttemptId::new()).await);
        client.close();
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn dropping_auth_aborts_callbacks_cleans_captures_and_cancels_same_connection()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, _events, peer, mut inbox) = setup().await?;
        let auth = command(AuthOperation::Login);
        let attempt_id = auth.attempt_id;
        let marker = Arc::new(());
        let weak = Arc::downgrade(&marker);
        let captured = client.clone();
        let (started, mut entered) = tokio::sync::mpsc::channel(1);
        let handler = Interaction::new(move |_| {
            let marker = marker.clone();
            let captured = captured.clone();
            let started = started.clone();
            async move {
                let _retain = (marker, captured);
                started.send(()).await.expect("test receiver open");
                std::future::pending::<Result<InteractionOutcome, moly_protocol::ProtocolError>>()
                    .await
            }
        });
        let authenticating = client.clone();
        let task =
            tokio::spawn(async move { authenticating.authenticate(auth, Some(handler)).await });
        let (id, _, _) = request(&mut inbox).await?;
        let reverse = peer.clone();
        let callback = tokio::spawn(async move { interaction(&reverse, attempt_id).await });
        entered.recv().await.ok_or("callback not entered")?;
        task.abort();
        assert!(task.await.is_err());
        unavailable(callback.await?);
        assert!(
            weak.upgrade().is_none(),
            "aborted callback must release strong captures"
        );
        let (cancel_id, method, params) = request(&mut inbox).await?;
        assert_eq!(method, "auth.cancel");
        assert_eq!(params["attempt_id"], json!(attempt_id));
        peer.respond(cancel_id, Ok(Value::Null)).await?;
        // A late terminal response is ignored and cannot resurrect the handler.
        peer.respond(id, Ok(status(attempt_id))).await?;
        unavailable(interaction(&peer, attempt_id).await);
        let reading = client.clone();
        let task = tokio::spawn(async move { reading.config().await });
        let (id, method, _) = request(&mut inbox).await?;
        assert_eq!(method, "config.get");
        peer.respond(id, Ok(json!({"revision":7,"config":null})))
            .await?;
        task.await??;
        client.close();
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn undeliverable_drop_cancellation_closes_the_connection() -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, mut events, peer, mut inbox) = setup().await?;
        let authenticating = client.clone();
        let task = tokio::spawn(async move {
            authenticating
                .authenticate(command(AuthOperation::Login), None)
                .await
        });
        request(&mut inbox).await?;
        task.abort();
        assert!(task.await.is_err());
        let (id, method, _) = request(&mut inbox).await?;
        assert_eq!(method, "auth.cancel");
        peer.respond(
            id,
            Err(moly_protocol::ProtocolError::new(
                "unknown_method",
                "cancel unsupported",
            )),
        )
        .await?;
        client.closed().await;
        assert!(events.recv().await.is_none());
        assert!(inbox.recv().await.is_none());
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn explicit_cancel_withdraws_presentation_but_preserves_terminal_response()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, _events, peer, mut inbox) = setup().await?;
        let auth = command(AuthOperation::Login);
        let attempt_id = auth.attempt_id;
        let handler = Interaction::new(|_| async { Ok(InteractionOutcome::Opened) });
        let authenticating = client.clone();
        let task =
            tokio::spawn(async move { authenticating.authenticate(auth, Some(handler)).await });
        let (id, _, _) = request(&mut inbox).await?;
        let cancelling = client.clone();
        let cancel = tokio::spawn(async move { cancelling.cancel_auth(attempt_id).await });
        let (cancel_id, method, params) = request(&mut inbox).await?;
        assert_eq!(method, "auth.cancel");
        assert_eq!(params["attempt_id"], json!(attempt_id));
        peer.respond(cancel_id, Ok(Value::Null)).await?;
        cancel.await??;
        unavailable(interaction(&peer, attempt_id).await);
        assert!(
            !task.is_finished(),
            "cancel acknowledgment is not the auth terminal result"
        );
        peer.respond(
            id,
            Err(moly_protocol::ProtocolError::new(
                "auth_cancelled",
                "cancelled",
            )),
        )
        .await?;
        assert!(matches!(task.await?, Err(Error::Remote(error)) if error.code == "auth_cancelled"));
        client.close();
        Ok::<_, TestError>(())
    })
    .await?
}

#[tokio::test]
async fn failed_auth_releases_handler_and_mismatched_result_closes() -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, _events, peer, mut inbox) = setup().await?;
        let auth = command(AuthOperation::Login);
        let attempt_id = auth.attempt_id;
        let marker = Arc::new(());
        let weak = Arc::downgrade(&marker);
        let captured = client.clone();
        let handler = Interaction::new(move |_| {
            let _ = (&marker, &captured);
            async { Ok(InteractionOutcome::Declined) }
        });
        let authenticating = client.clone();
        let task =
            tokio::spawn(async move { authenticating.authenticate(auth, Some(handler)).await });
        let (id, _, _) = request(&mut inbox).await?;
        peer.respond(
            id,
            Err(moly_protocol::ProtocolError::new(
                "auth_required",
                "login failed",
            )),
        )
        .await?;
        assert!(matches!(task.await?, Err(Error::Remote(_))));
        assert!(weak.upgrade().is_none());
        unavailable(interaction(&peer, attempt_id).await);
        let authenticating = client.clone();
        let task = tokio::spawn(async move {
            authenticating
                .authenticate(command(AuthOperation::Status), None)
                .await
        });
        let (id, _, _) = request(&mut inbox).await?;
        peer.respond(id, Ok(status(AuthAttemptId::new()))).await?;
        assert!(matches!(
            task.await?,
            Err(Error::Frame("mismatched auth attempt"))
        ));
        client.closed().await;
        Ok::<_, TestError>(())
    })
    .await?
}
