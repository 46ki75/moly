//! Public callback correlation and borrowed credential-slot authority.
use super::support::*;
use moly_provider_client::{
    Interaction, ProviderClient,
    protocol::{
        ProtocolError,
        auth::{AuthOperation, InteractionOutcome},
    },
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn opened() -> Interaction {
    Interaction::new(|_| async { Ok(InteractionOutcome::Opened) })
}

#[tokio::test]
async fn login_status_logout_use_only_the_borrowed_slot_and_sdk_binds_callback_ids() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let config = inputs.auth("sequential_login", None)?;
        let client = ProviderClient;
        let mut selected = None;
        let unselected = Some(OTHER.to_owned());
        let status = client
            .authenticate(
                &config,
                auth_request(AuthOperation::Status),
                &mut selected,
                None,
            )
            .await?;
        assert!(
            !status.authenticated,
            "stale request credential must not authenticate an empty slot"
        );
        let login = auth_request(AuthOperation::Login);
        let attempt = login.attempt_id;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let interaction = Interaction::new(move |request| {
            assert_eq!(request.attempt_id, attempt);
            assert_eq!(request.url, URL);
            count.fetch_add(1, Ordering::SeqCst);
            // Hosts return only an outcome. The SDK, not the caller, builds the
            // correlated wire response from the accepted authentication attempt.
            async { Ok(InteractionOutcome::Opened) }
        });
        let status = client
            .authenticate(&config, login, &mut selected, Some(interaction))
            .await?;
        assert!(status.authenticated);
        assert_eq!(status.attempt_id, attempt);
        assert_eq!(selected.as_deref(), Some(LOGIN));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        redacted(&serde_json::to_string(&status)?);
        assert!(
            client
                .authenticate(
                    &config,
                    auth_request(AuthOperation::Status),
                    &mut selected,
                    None
                )
                .await?
                .authenticated
        );
        let status = client
            .authenticate(
                &config,
                auth_request(AuthOperation::Logout),
                &mut selected,
                None,
            )
            .await?;
        assert!(!status.authenticated);
        assert_eq!(
            status.revocation_confirmed,
            Some(false),
            "local logout is not proof of revocation"
        );
        assert!(selected.is_none());
        assert!(
            !client
                .authenticate(
                    &config,
                    auth_request(AuthOperation::Status),
                    &mut selected,
                    None
                )
                .await?
                .authenticated
        );
        assert_eq!(unselected.as_deref(), Some(OTHER));
        let records = inputs.records()?;
        let auths = auth_messages(&records, "provider.auth");
        assert_eq!(auths.len(), 5, "one child per operation, no retry");
        assert_eq!(auth_messages(&records, "initialize").len(), 5);
        for auth in &auths {
            assert_eq!(auth["ppid"], std::process::id());
            assert_eq!(auth["message"]["params"]["options"], config.options);
        }
        assert!(auths[0]["message"]["params"]["credential"].is_null());
        assert!(auths[1]["message"]["params"]["credential"].is_null());
        assert_eq!(auths[2]["message"]["params"]["credential"], LOGIN);
        assert_eq!(auths[3]["message"]["params"]["credential"], LOGIN);
        assert!(auths[4]["message"]["params"]["credential"].is_null());
        for pair in auths.windows(2) {
            assert_ne!(pair[0]["instance_id"], pair[1]["instance_id"]);
        }
        let interactions = auth_messages(&records, "host.interact");
        assert_eq!(interactions.len(), 2);
        for interaction in interactions {
            let replies: Vec<_> = records
                .iter()
                .filter(|record| {
                    record["instance_id"] == interaction["instance_id"]
                        && record["direction"] == "received"
                        && record["message"]["type"] == "response"
                        && record["message"]["id"] == interaction["message"]["id"]
                })
                .collect();
            assert_eq!(replies.len(), 1);
            assert_eq!(
                replies[0]["message"]["result"],
                json!({"attempt_id":attempt,"outcome":"opened"})
            );
        }
        assert_eq!(auth_messages(&records, "host.credential.replace").len(), 2);
        assert!(!serde_json::to_string(&records)?.contains(STALE));
        assert!(!serde_json::to_string(&records)?.contains(OTHER));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn headless_declined_unavailable_and_callback_errors_preserve_existing_credentials()
-> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let config = inputs.auth("login", None)?;
        let mut selected = Some(OLD.into());
        for (callback, code) in [
            (None, "interaction_unavailable"),
            (
                Some(Interaction::new(|_| async {
                    Ok(InteractionOutcome::Declined)
                })),
                "auth_declined",
            ),
            (
                Some(Interaction::new(|_| async {
                    Ok(InteractionOutcome::Unavailable)
                })),
                "interaction_unavailable",
            ),
            (
                Some(Interaction::new(|_| async {
                    Err(ProtocolError::new(
                        "mpp-wire-secret-sentinel",
                        "auth-secret-callback-sentinel",
                    ))
                })),
                "interaction_unavailable",
            ),
        ] {
            error_code(
                ProviderClient
                    .authenticate(
                        &config,
                        auth_request(AuthOperation::Login),
                        &mut selected,
                        callback,
                    )
                    .await
                    .expect_err("presentation is not authentication"),
                code,
            );
            assert_eq!(selected.as_deref(), Some(OLD));
        }
        let records = inputs.records()?;
        assert_eq!(
            auth_messages(&records, "provider.auth").len(),
            4,
            "no automatic retry or fallback"
        );
        assert!(auth_messages(&records, "host.credential.replace").is_empty());
        assert!(
            auth_messages(&records, "provider.auth")
                .iter()
                .all(|record| record["message"]["params"]["credential"] == OLD)
        );
        for record in &records {
            if record["direction"] == "received" && record["message"]["type"] == "error" {
                redacted(&serde_json::to_string(&record["message"])?);
            }
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn invalid_auth_correlations_urls_and_reverse_ids_fail_closed() -> TestResult {
    bounded(async {
        for (scenario, code, callback_count) in [
            ("wrong_status", "provider_protocol", 0),
            ("wrong_interaction", "provider_protocol", 0),
            ("http_url", "provider_protocol", 0),
            ("control_url", "provider_protocol", 0),
            ("oversized_url", "provider_protocol", 0),
            ("zero_host_id", "provider_protocol", 0),
            ("repeat_host_id", "provider_protocol", 1),
            ("error", "auth_failed", 0),
            ("unknown_auth", "auth_unsupported", 0),
        ] {
            let inputs = Inputs::new().await?;
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            let callback = Interaction::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                async { Ok(InteractionOutcome::Opened) }
            });
            let mut selected = Some(OLD.into());
            error_code(
                ProviderClient
                    .authenticate(
                        &inputs.auth(scenario, None)?,
                        auth_request(AuthOperation::Login),
                        &mut selected,
                        Some(callback),
                    )
                    .await
                    .expect_err("invalid auth must fail"),
                code,
            );
            assert_eq!(selected.as_deref(), Some(OLD));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                callback_count,
                "reject invalid interaction before callback: {scenario}"
            );
            let records = inputs.records()?;
            assert_eq!(auth_messages(&records, "initialize").len(), 1);
            assert_eq!(
                auth_messages(&records, "provider.auth").len(),
                1,
                "no retry: {scenario}"
            );
            assert!(auth_messages(&records, "host.credential.replace").is_empty());
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn validation_status_and_unselected_steps_cannot_write_or_interact() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let client = ProviderClient;
        let mut selected = Some(OLD.into());
        client
            .validate(&inputs.auth("validate_write", None)?)
            .await?;
        let status = client
            .authenticate(
                &inputs.auth("status_write", None)?,
                auth_request(AuthOperation::Status),
                &mut selected,
                Some(opened()),
            )
            .await?;
        assert!(status.authenticated);
        assert_eq!(selected.as_deref(), Some(OLD));
        client
            .authenticate(
                &inputs.auth("status_interact", None)?,
                auth_request(AuthOperation::Status),
                &mut selected,
                Some(Interaction::new(|_| async {
                    panic!("status cannot request interaction")
                })),
            )
            .await?;
        assert_eq!(selected.as_deref(), Some(OLD));
        client
            .step(&inputs.auth("step_write", None)?, request(), None)
            .await?;
        assert_eq!(selected.as_deref(), Some(OLD));
        client
            .step(
                &inputs.auth("step_write", None)?,
                request(),
                Some(&mut selected),
            )
            .await?;
        assert_eq!(
            selected.as_deref(),
            Some(LOGIN),
            "selected model scope can refresh the borrowed slot"
        );
        error_code(
            client
                .step(
                    &inputs.auth("step_interact", None)?,
                    request(),
                    Some(&mut selected),
                )
                .await
                .expect_err("model steps have no interaction capability"),
            "auth_required",
        );
        let records = inputs.records()?;
        let writes = auth_messages(&records, "host.credential.replace");
        assert_eq!(writes.len(), 4);
        for (index, write) in writes.iter().enumerate() {
            let replies: Vec<_> = records
                .iter()
                .filter(|record| {
                    record["instance_id"] == write["instance_id"]
                        && record["direction"] == "received"
                        && record["message"]["id"] == write["message"]["id"]
                        && matches!(
                            record["message"]["type"].as_str(),
                            Some("response" | "error")
                        )
                })
                .collect();
            assert_eq!(replies.len(), 1);
            if index < 3 {
                assert_eq!(
                    replies[0]["message"]["error"]["code"],
                    "host_service_unavailable"
                );
            } else {
                assert_eq!(replies[0]["message"]["type"], "response");
            }
        }
        let validations = auth_messages(&records, "provider.validate");
        assert_eq!(validations.len(), 1);
        assert_eq!(
            validations[0]["message"]["params"],
            json!({"scenario":"validate_write"})
        );
        let steps = auth_messages(&records, "provider.step");
        assert_eq!(steps.len(), 3);
        assert!(steps[0]["message"]["params"]["credential"].is_null());
        assert_eq!(steps[1]["message"]["params"]["credential"], OLD);
        assert_eq!(steps[2]["message"]["params"]["credential"], LOGIN);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["direction"] == "received"
                    && record["message"]["error"]["code"] == "interaction_unavailable")
                .count(),
            2
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn credential_commit_survives_operation_error_and_utf8_size_limit_is_enforced() -> TestResult
{
    bounded(async {
        let inputs = Inputs::new().await?;
        let mut selected = Some(OLD.into());
        ProviderClient
            .step(
                &inputs.mpp("credential_limit", None)?,
                request(),
                Some(&mut selected),
            )
            .await?;
        assert_eq!(
            selected.as_deref(),
            Some(OLD),
            "64 KiB limit counts UTF-8 bytes, not scalar values"
        );
        error_code(
            ProviderClient
                .step(
                    &inputs.mpp("commit_error", None)?,
                    request(),
                    Some(&mut selected),
                )
                .await
                .expect_err("error after replacement"),
            "auth_failed",
        );
        let committed: Value =
            serde_json::from_str(selected.as_deref().ok_or("missing committed slot")?)?;
        assert_eq!(
            committed["generation"], 2,
            "acknowledged replacement is not rolled back on operation failure"
        );
        let records = inputs.records()?;
        assert_eq!(auth_messages(&records, "provider.step").len(), 2);
        assert_eq!(auth_messages(&records, "host.credential.replace").len(), 2);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn handshake_rejects_reverse_services_and_operation_bounds_reverse_calls() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        error_code(
            ProviderClient
                .validate(&inputs.mpp("handshake_reverse", None)?)
                .await
                .expect_err("no reverse service during handshake"),
            "provider_protocol",
        );
        assert!(auth_messages(&inputs.records()?, "provider.validate").is_empty());
        let inputs = Inputs::new().await?;
        error_code(
            ProviderClient
                .step(&inputs.mpp("reverse_limit", None)?, request(), None)
                .await
                .expect_err("only 64 sequential reverse requests"),
            "provider_protocol",
        );
        let records = inputs.records()?;
        assert_eq!(auth_messages(&records, "host.unknown").len(), 65);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["direction"] == "received"
                    && record["message"]["type"] == "error")
                .count(),
            64
        );
        assert_eq!(auth_messages(&records, "provider.step").len(), 1);
        Ok(())
    })
    .await
}
