//! Deterministic connection-scoped authentication authority checks.
use super::*;

#[tokio::test]
async fn cancel_cannot_cross_connections_or_resurrect_a_finished_attempt()
-> Result<(), ProtocolError> {
    let server = Server::new()?;
    let (owner, _effects) = Connection::new();
    let (other, _other_effects) = Connection::new();
    let attempt = AuthAttemptId::new();
    let (guard, cancel) = owner.begin_auth(attempt)?;
    let params = json!({"attempt_id":attempt});
    assert_eq!(
        server
            .request(&other, "auth.cancel", params.clone())
            .await
            .expect_err("foreign attempt")
            .code,
        "not_active"
    );
    assert!(!cancel.is_cancelled());
    assert!(owner.begin_auth(attempt).is_err());
    server
        .request(&owner, "auth.cancel", params.clone())
        .await?;
    assert!(cancel.is_cancelled());
    drop(guard);
    assert_eq!(
        server
            .request(&owner, "auth.cancel", params)
            .await
            .expect_err("finished")
            .code,
        "not_active"
    );
    assert!(
        owner.begin_auth(attempt).is_err(),
        "attempts cannot be reused after completion"
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_before_command_acceptance_fences_delayed_auth() -> Result<(), ProtocolError> {
    let server = Server::new()?;
    let (connection, _effects) = Connection::new();
    let attempt = AuthAttemptId::new();
    assert_eq!(
        server
            .request(&connection, "auth.cancel", json!({"attempt_id":attempt}))
            .await
            .expect_err("not active yet")
            .code,
        "not_active"
    );
    let result = connection.begin_auth(attempt);
    assert!(
        matches!(result, Err(ref error) if error.code == "auth_cancelled"),
        "a delayed request must not start after cancellation"
    );
    Ok(())
}

#[test]
fn dropped_operations_release_capacity_without_reusing_identities() -> Result<(), ProtocolError> {
    let (connection, _effects) = Connection::new();
    let mut guards = Vec::new();
    for _ in 0..8 {
        guards.push(connection.begin_auth(AuthAttemptId::new())?.0);
    }
    assert!(connection.begin_auth(AuthAttemptId::new()).is_err());
    let removed = guards.pop().expect("eight active operations");
    drop(removed);
    let (guard, cancel) = connection.begin_auth(AuthAttemptId::new())?;
    drop(guard);
    assert!(cancel.is_cancelled());
    drop(guards);
    assert!(connection.auth.lock().expect("registry").active.is_empty());
    Ok(())
}

#[tokio::test]
async fn malformed_auth_shapes_and_stale_config_fail_before_provider_launch()
-> Result<(), ProtocolError> {
    let server = Server::new()?;
    let (connection, _effects) = Connection::new();
    let id = AuthAttemptId::new();
    for (method, params) in [
        ("auth.cancel", json!([id])),
        ("provider.auth", json!([id, "login", 0])),
    ] {
        assert_eq!(
            server
                .request(&connection, method, params)
                .await
                .expect_err("must reject positional arrays")
                .code,
            "invalid_params"
        );
    }
    assert_eq!(
        server
            .request(
                &connection,
                "provider.auth",
                json!({"attempt_id":id,"operation":"login","config_revision":1})
            )
            .await
            .expect_err("stale config")
            .code,
        "revision_conflict"
    );
    assert_eq!(
        server
            .request(
                &connection,
                "provider.auth",
                json!({"attempt_id":id,"operation":"login","config_revision":0})
            )
            .await
            .expect_err("no config")
            .code,
        "not_configured"
    );
    assert!(connection.auth.lock().expect("registry").seen.is_empty());
    Ok(())
}
