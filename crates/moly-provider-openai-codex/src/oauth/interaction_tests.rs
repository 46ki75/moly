//! Interaction responses are presentation outcomes, never authentication proof.

use moly_protocol::{AuthAttemptId, Body};
use serde_json::json;

use super::tests::registered;
use crate::service::Service;
use crate::test_support::{self as fixture, Peer, Reply, Server, TestResult};

#[tokio::test]
async fn declined_unavailable_or_wrong_attempt_never_exchanges_or_commits() -> TestResult {
    for outcome in ["declined", "unavailable", "wrong_attempt"] {
        let server = Server::start(|_, origin| Reply::json(fixture::discovery(origin))).await?;
        let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
        peer.initialize().await?;
        let attempt = AuthAttemptId::new();
        peer.request(2,"provider.auth",json!({"attempt_id":attempt,"operation":"login","options":fixture::options(),"credential":null})).await?;
        let Body::Request { id, method, .. } = peer.recv().await?.body else {
            return Err("interaction".into());
        };
        assert_eq!(method, "host.interact");
        let response_attempt = if outcome == "wrong_attempt" {
            AuthAttemptId::new()
        } else {
            attempt
        };
        let presented = if outcome == "wrong_attempt" {
            "opened"
        } else {
            outcome
        };
        peer.response(
            id,
            json!({"attempt_id":response_attempt,"outcome":presented}),
        )
        .await?;
        assert!(
            matches!(peer.recv().await?.body,Body::Error { error,.. } if error.code == "auth_interaction_unavailable" || error.code == "provider_protocol")
        );
        let requests = server.requests.lock().map_err(|_| "requests")?;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/discovery");
    }
    Ok(())
}

#[test]
fn issued_registration_is_nonsecret_and_binds_host_and_issuer() -> TestResult {
    let value = serde_json::to_value(registered())?;
    assert_eq!(value.as_object().ok_or("registration")?.len(), 5);
    for field in ["access_token", "refresh_token", "id_token", "email"] {
        assert!(value.get(field).is_none());
    }
    let mut extra = value;
    extra["access_token"] = json!("not-permitted-in-options");
    assert!(serde_json::from_value::<crate::config::Registration>(extra).is_err());
    assert!(
        registered()
            .validate(
                "urn:uuid:00000000-0000-4000-8000-000000000002",
                crate::config::ISSUER
            )
            .is_err()
    );
    assert!(
        registered()
            .validate(&registered().host_id, "https://another.invalid")
            .is_err()
    );
    Ok(())
}
