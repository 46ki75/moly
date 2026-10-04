//! Cancellation tests observe Provider-held socket EOF, not task cancellation alone.
use super::support::*;
use moly_provider_client::{
    Interaction, ProviderClient,
    protocol::{
        ProtocolError,
        auth::{AuthOperation, InteractionOutcome},
    },
};
use serde_json::Value;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

async fn cancel_after_gate<T>(
    operation: impl Future<Output = Result<T, ProtocolError>>,
    listener: &TcpListener,
    phase: &str,
) -> Result<Value, TestError> {
    // A boxed future is actually destroyed by drop. Dropping only a pinned
    // reference would leave the underlying operation/child alive until scope exit.
    let mut operation = Box::pin(operation);
    let (mut child, ready) = tokio::select! {
        result = &mut operation => return Err(format!("operation ended before gate: {}", result.is_ok()).into()),
        observed = gate(listener) => observed?,
    };
    assert_eq!(ready["phase"], phase);
    child_alive(&mut child).await?;
    drop(operation);
    child_stopped(&mut child).await?;
    Ok(ready)
}

#[tokio::test]
async fn dropping_operations_during_handshake_and_blocked_reply_kills_owned_children() -> TestResult
{
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let client = ProviderClient;
        let handshake = inputs.mpp("handshake_gate", Some(port))?;
        cancel_after_gate(client.validate(&handshake), &listener, "initialize").await?;
        let mut selected = Some(OLD.into());
        cancel_after_gate(
            client.authenticate(
                &handshake,
                auth_request(AuthOperation::Login),
                &mut selected,
                None,
            ),
            &listener,
            "initialize",
        )
        .await?;
        assert_eq!(selected.as_deref(), Some(OLD));
        let validate = inputs.mpp("validate_gate", Some(port))?;
        cancel_after_gate(client.validate(&validate), &listener, "validate").await?;
        let step = inputs.mpp("read_gate", Some(port))?;
        cancel_after_gate(
            client.step(&step, request(), Some(&mut selected)),
            &listener,
            "step",
        )
        .await?;
        assert_eq!(selected.as_deref(), Some(OLD));
        let records = inputs.records()?;
        assert_eq!(
            auth_messages(&records, "initialize").len(),
            4,
            "no delayed retries after cancellation"
        );
        assert_eq!(auth_messages(&records, "provider.validate").len(), 1);
        assert_eq!(auth_messages(&records, "provider.step").len(), 1);
        assert!(auth_messages(&records, "provider.auth").is_empty());
        client.validate(&inputs.model("valid")?).await?;
        assert_eq!(
            model_requests(&inputs.records()?, "provider.validate").len(),
            1,
            "cancellation does not poison the stateless host"
        );
        Ok(())
    })
    .await
}

struct CallbackLifetime(Arc<AtomicUsize>);
impl Drop for CallbackLifetime {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn dropping_auth_drops_the_inline_callback_and_terminates_its_waiting_child() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let config = inputs.auth("pending", Some(listener.local_addr()?.port()))?;
        let mut selected = Some(OLD.into());
        let dropped = Arc::new(AtomicUsize::new(0));
        let lifetime = dropped.clone();
        let (entered, entry) = oneshot::channel();
        let entered = Arc::new(std::sync::Mutex::new(Some(entered)));
        let callback = Interaction::new(move |_| {
            let lifetime = CallbackLifetime(lifetime.clone());
            entered.lock().expect("test signal lock").take().expect("one callback")
                .send(()).expect("entry receiver exists");
            async move {
                let _lifetime = lifetime;
                std::future::pending::<Result<InteractionOutcome, ProtocolError>>().await
            }
        });
        let mut operation = Box::pin(ProviderClient.authenticate(&config, auth_request(AuthOperation::Login), &mut selected, Some(callback)));
        let (mut child, ready) = tokio::select! {
            result = &mut operation => return Err(format!("auth ended before callback gate: {}", result.is_ok()).into()),
            observed = gate(&listener) => observed?,
        };
        assert_eq!(ready["phase"], "interaction");
        tokio::select! {
            result = &mut operation => return Err(format!("auth ended before callback entry: {}", result.is_ok()).into()),
            observed = entry => observed?,
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 0, "callback must remain pending until cancellation");
        drop(operation);
        assert_eq!(dropped.load(Ordering::SeqCst), 1, "callback is inline, not a detached worker");
        child_stopped(&mut child).await?;
        assert_eq!(selected.as_deref(), Some(OLD));
        let records = inputs.records()?;
        assert_eq!(auth_messages(&records, "provider.auth").len(), 1);
        assert!(auth_messages(&records, "host.credential.replace").is_empty());
        Ok(())
    }).await
}

#[tokio::test]
async fn opened_callback_is_not_authentication_and_cancellation_preserves_old_slot() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let config = inputs.auth("pending", Some(listener.local_addr()?.port()))?;
        let mut selected = Some(OLD.into());
        let mut operation = Box::pin(ProviderClient.authenticate(&config, auth_request(AuthOperation::Login), &mut selected,
            Some(Interaction::new(|_| async { Ok(InteractionOutcome::Opened) }))));
        let (mut child, _) = tokio::select! {
            result = &mut operation => return Err(format!("auth ended before gate: {}", result.is_ok()).into()),
            observed = gate(&listener) => observed?,
        };
        let mut line = String::new();
        let count = tokio::select! {
            result = &mut operation => return Err(format!("browser presentation prematurely completed auth: {}", result.is_ok()).into()),
            count = child.read_line(&mut line) => count?,
        };
        assert_ne!(count, 0);
        assert_eq!(serde_json::from_str::<Value>(&line)?["phase"], "presented");
        child_alive(&mut child).await?;
        drop(operation);
        child_stopped(&mut child).await?;
        assert_eq!(selected.as_deref(), Some(OLD));
        assert!(auth_messages(&inputs.records()?, "host.credential.replace").is_empty());
        Ok(())
    }).await
}

#[tokio::test]
async fn acknowledged_auth_and_model_replacements_survive_cancellation() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let mut selected = Some(OLD.into());
        let unselected = Some(OTHER.to_owned());
        let config = inputs.auth("commit_then_gate", Some(port))?;
        cancel_after_gate(
            ProviderClient.authenticate(
                &config,
                auth_request(AuthOperation::Login),
                &mut selected,
                None,
            ),
            &listener,
            "committed",
        )
        .await?;
        assert_eq!(
            selected.as_deref(),
            Some(LOGIN),
            "reverse response acknowledges an immediate commit"
        );
        let config = inputs.mpp("step_commit_gate", Some(port))?;
        let ready = cancel_after_gate(
            ProviderClient.step(&config, request(), Some(&mut selected)),
            &listener,
            "committed",
        )
        .await?;
        assert_eq!(ready["request"]["params"]["credential"], LOGIN);
        let committed: Value =
            serde_json::from_str(selected.as_deref().ok_or("missing refreshed slot")?)?;
        assert_eq!(
            committed["generation"], 2,
            "cancel does not roll back model refresh"
        );
        assert_eq!(unselected.as_deref(), Some(OTHER));
        let records = inputs.records()?;
        assert_eq!(auth_messages(&records, "host.credential.replace").len(), 2);
        assert_eq!(auth_messages(&records, "initialize").len(), 2);
        assert_eq!(auth_messages(&records, "provider.auth").len(), 1);
        assert_eq!(auth_messages(&records, "provider.step").len(), 1);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn successful_and_failed_steps_clean_up_children_that_remain_alive_after_reply() -> TestResult
{
    bounded(async {
        for scenario in ["success_gate", "error_gate"] {
            let inputs = Inputs::new().await?;
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let config = inputs.mpp(scenario, Some(listener.local_addr()?.port()))?;
            let mut operation = Box::pin(ProviderClient.step(&config, request(), None));
            let (mut child, _) = tokio::select! {
                result = &mut operation => return Err(format!("step ended before gate: {}", result.is_ok()).into()),
                observed = gate(&listener) => observed?,
            };
            child_alive(&mut child).await?;
            child.get_mut().write_all(b"+").await?;
            let result = operation.await;
            if scenario == "success_gate" { result?; }
            else { error_code(result.expect_err("untrusted provider error"), "provider_error"); }
            child_stopped(&mut child).await?;
            let records = inputs.records()?;
            assert_eq!(auth_messages(&records, "initialize").len(), 1);
            assert_eq!(auth_messages(&records, "provider.step").len(), 1);
        }
        Ok(())
    }).await
}
