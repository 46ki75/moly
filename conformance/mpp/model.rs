//! Language-neutral model frames, tool-batch validation, and host-owned history.
use super::support::*;
use moly_provider_client::{
    ProviderClient,
    protocol::{
        MAX_FRAME_BYTES, ModelCallId, RunId,
        model::{ModelMessage, ModelStep, ProviderMetadata},
    },
};
use serde_json::json;
use std::collections::HashSet;

#[tokio::test]
async fn public_stateless_api_derives_options_credentials_and_explicit_environment() -> TestResult {
    fn public_traits<T: Default + Clone + Copy + Send + Sync>() {}
    public_traits::<ProviderClient>();
    bounded(async {
        let inputs = Inputs::new().await?;
        let client: ProviderClient = Default::default();
        let config = inputs.model("valid")?;
        assert!(std::path::Path::new(&config.command.executable).is_absolute());
        client.validate(&config).await?;
        let mut selected = Some(OLD.into());
        let unselected = Some(OTHER.to_owned());
        let model_request = request();
        let context = serde_json::to_value(model_request.context)?;
        let messages = serde_json::to_value(&model_request.messages)?;
        let step = client.step(&config, model_request, Some(&mut selected)).await?;
        assert!(matches!(step, ModelStep::Completed { text, metadata: None } if text == "independent provider complete"));
        client.step(&config, request(), None).await?;
        assert_eq!(selected.as_deref(), Some(OLD));
        assert_eq!(unselected.as_deref(), Some(OTHER));
        let records = inputs.records()?;
        let initialized = model_requests(&records, "initialize");
        assert_eq!(initialized.len(), 3, "one fresh child per explicit operation");
        let instances: HashSet<_> = initialized.iter().map(|record| record["instance_id"].clone()).collect();
        assert_eq!(instances.len(), 3, "no child reuse");
        for record in &records {
            assert_eq!(record["ppid"], std::process::id(), "SDK host must launch the peer directly");
            assert_eq!(record["environment"], serde_json::to_value(&config.command.env)?, "no ambient environment inheritance");
            assert_eq!(record["request"]["version"], 1);
        }
        let validations = model_requests(&records, "provider.validate");
        assert_eq!(validations.len(), 1);
        assert_eq!(validations[0]["request"]["params"], config.options);
        let steps = model_requests(&records, "provider.step");
        assert_eq!(steps.len(), 2, "SDK has no agent loop, implicit validation, or retry");
        assert_eq!(steps[0]["request"]["params"]["options"], config.options);
        assert_eq!(steps[0]["request"]["params"]["credential"], OLD);
        assert_eq!(steps[0]["request"]["params"]["context"], context);
        assert_eq!(steps[0]["request"]["params"]["messages"], messages);
        assert!(steps[1]["request"]["params"]["credential"].is_null(), "no selected slot means no credential, even with stale request data");
        assert!(!serde_json::to_string(&records)?.contains(STALE));
        assert!(!serde_json::to_string(&records)?.contains(OTHER));
        Ok(())
    }).await
}

#[tokio::test]
async fn conversation_preserves_opaque_metadata_and_structured_tool_failure() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let client = ProviderClient;
        let mut config = inputs.mpp("conversation", None)?;
        // Shell-looking content is a literal argument, not expanded or executed.
        let literal = "literal $HOME; $(exit 9) α";
        config.command.args.push(literal.into());
        let metadata = ProviderMetadata {
            format: "independent.mpp.opaque.v1".into(),
            value: json!({"reasoning":[null,{"private":"opaque α\nreplay"}],"future":true}),
        };
        let mut context = request();
        let session = context.context.session_id;
        let run = context.context.run_id;
        let first_call = context.context.model_call_id;
        let first = client.step(&config, context.clone(), None).await?;
        let ModelStep::AwaitHostTools { text, calls, metadata: received } = first else {
            return Err("expected host-tool request".into());
        };
        assert_eq!(text.as_deref(), Some("before tool"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "provider-call-1");
        assert_eq!(calls[0].name, "component_echo");
        assert_eq!(calls[0].arguments, json!({"nested":[true,null,7]}));
        assert_eq!(received, Some(metadata.clone()));
        assert_eq!(auth_messages(&inputs.records()?, "provider.step").len(), 1, "return tool intent without executing or continuing it");
        context.messages.push(ModelMessage::Assistant { text, tool_calls: calls, metadata: received });
        let output = json!({"error":{"code":"tool_file_not_found","message":"Requested file was not found"},"opaque":[null,7]});
        context.messages.push(ModelMessage::ToolResult { call_id: "provider-call-1".into(), output });
        context.context.model_call_id = ModelCallId::new();
        config.options["stage"] = json!(1);
        let second_call = context.context.model_call_id;
        let second = client.step(&config, context.clone(), None).await?;
        let ModelStep::Completed { text, metadata: received } = second else {
            return Err("expected tool continuation completion".into());
        };
        assert_eq!(text, "tool continued");
        assert_eq!(received, Some(metadata.clone()));
        context.messages.push(ModelMessage::Assistant { text: Some(text), tool_calls: vec![], metadata: received });
        context.messages.push(ModelMessage::User { text: "next turn".into() });
        context.context.run_id = RunId::new();
        context.context.model_call_id = ModelCallId::new();
        config.options["stage"] = json!(2);
        let third = client.step(&config, context.clone(), None).await?;
        assert!(matches!(third, ModelStep::Completed { text, metadata: Some(received) } if text.is_empty() && received == metadata));
        let records = inputs.records()?;
        let steps = auth_messages(&records, "provider.step");
        assert_eq!(steps.len(), 3, "exactly one inference per call");
        let instances: HashSet<_> = steps.iter().map(|record| record["instance_id"].clone()).collect();
        assert_eq!(instances.len(), 3);
        for step in &steps {
            let argv = step["argv"].as_array().ok_or("missing fixture argv")?;
            assert_eq!(argv.last(), Some(&json!(literal)), "literal arguments must survive without a shell");
            assert_eq!(step["message"]["params"]["context"]["session_id"], json!(session));
            assert!(step["message"]["params"]["credential"].is_null());
        }
        assert_eq!(steps[0]["message"]["params"]["context"]["run_id"], json!(run));
        assert_eq!(steps[1]["message"]["params"]["context"]["run_id"], json!(run));
        assert_eq!(steps[0]["message"]["params"]["context"]["model_call_id"], json!(first_call));
        assert_eq!(steps[1]["message"]["params"]["context"]["model_call_id"], json!(second_call));
        assert_eq!(steps[2]["message"]["params"]["messages"], serde_json::to_value(&context.messages)?);
        Ok(())
    }).await
}

#[tokio::test]
async fn invalid_handshake_and_validation_fail_without_retry_and_redact_peer_data() -> TestResult {
    bounded(async {
        for (scenario, code, validation_count) in [
            ("bad_role", "provider_protocol", 0),
            ("bad_version", "provider_protocol", 0),
            ("bad_envelope_version", "provider_protocol", 0),
            ("bad_handshake_array", "provider_protocol", 0),
            ("validate_non_null", "provider_protocol", 1),
            ("validate_unknown_error", "provider_error", 1),
            ("validate_known_error", "invalid_config", 1),
        ] {
            let inputs = Inputs::new().await?;
            error_code(
                ProviderClient
                    .validate(&inputs.model(scenario)?)
                    .await
                    .expect_err("invalid peer must fail"),
                code,
            );
            let records = inputs.records()?;
            assert_eq!(
                model_requests(&records, "initialize").len(),
                1,
                "no initialization retry: {scenario}"
            );
            assert_eq!(
                model_requests(&records, "provider.validate").len(),
                validation_count
            );
            assert!(model_requests(&records, "provider.step").is_empty());
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn malformed_model_wire_and_tool_batches_fail_before_returning_tool_intent() -> TestResult {
    bounded(async {
        for (scenario, code) in [
            ("eof", "provider_unavailable"),
            ("malformed", "provider_protocol"),
            ("mismatched_id", "provider_protocol"),
            ("oversized", "provider_protocol"),
            ("unknown_type", "provider_protocol"),
            ("unknown_outcome", "provider_protocol"),
            ("metadata_array", "provider_protocol"),
            ("tool_call_arrays", "provider_protocol"),
            ("duplicate_ids", "provider_protocol"),
            ("unadvertised_tool", "provider_protocol"),
            ("unknown_error", "provider_error"),
            ("known_error", "invalid_secret"),
        ] {
            let inputs = Inputs::new().await?;
            error_code(
                ProviderClient
                    .step(&inputs.model(scenario)?, request(), None)
                    .await
                    .expect_err("invalid step must fail"),
                code,
            );
            let records = inputs.records()?;
            assert_eq!(model_requests(&records, "initialize").len(), 1);
            assert_eq!(
                model_requests(&records, "provider.step").len(),
                1,
                "no inference retry: {scenario}"
            );
        }
        for scenario in [
            "empty_batch",
            "too_many_calls",
            "empty_id",
            "arguments_array",
            "partial_eof",
            "invalid_utf8",
            "event",
            "missing_text",
            "null_text",
        ] {
            let inputs = Inputs::new().await?;
            error_code(
                ProviderClient
                    .step(&inputs.mpp(scenario, None)?, request(), None)
                    .await
                    .expect_err("invalid step must fail"),
                "provider_protocol",
            );
            assert_eq!(
                auth_messages(&inputs.records()?, "provider.step").len(),
                1,
                "no retry: {scenario}"
            );
        }
        let inputs = Inputs::new().await?;
        let mut no_tools = request();
        no_tools.tools.clear();
        error_code(
            ProviderClient
                .step(&inputs.model("conversation")?, no_tools, None)
                .await
                .expect_err("empty advertisement grants no tools"),
            "provider_protocol",
        );
        let inputs = Inputs::new().await?;
        let accepted = ProviderClient
            .step(&inputs.mpp("max_calls", None)?, request(), None)
            .await?;
        assert!(matches!(accepted, ModelStep::AwaitHostTools { calls, .. } if calls.len() == 32));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn resolved_command_and_outgoing_frame_limits_are_enforced() -> TestResult {
    bounded(async {
        let inputs = Inputs::new().await?;
        let config = inputs.model("valid")?;
        for invalid in 0..4 {
            let mut config = config.clone();
            match invalid {
                0 => config.command.executable = "python3".into(),
                1 => config.command.args.push("invalid\0argument".into()),
                2 => {
                    config
                        .command
                        .env
                        .insert("INVALID=KEY".into(), "value".into());
                }
                _ => {
                    config
                        .command
                        .env
                        .insert("INVALID_VALUE".into(), "value\0".into());
                }
            }
            error_code(
                ProviderClient
                    .validate(&config)
                    .await
                    .expect_err("invalid command must fail before launch"),
                "invalid_config",
            );
        }
        assert!(
            inputs.records()?.is_empty(),
            "invalid launch data must not spawn any Provider"
        );
        let mut missing = config.clone();
        missing.command.executable.push_str(".mpp-test-nonexistent");
        error_code(
            ProviderClient
                .validate(&missing)
                .await
                .expect_err("missing executable"),
            "provider_unavailable",
        );
        let mut oversized = request();
        oversized.messages = vec![ModelMessage::User {
            text: "x".repeat(MAX_FRAME_BYTES),
        }];
        error_code(
            ProviderClient
                .step(&config, oversized, None)
                .await
                .expect_err("oversized context must not be truncated"),
            "provider_request_too_large",
        );
        let records = inputs.records()?;
        assert_eq!(model_requests(&records, "initialize").len(), 1);
        assert!(model_requests(&records, "provider.step").is_empty());
        // The same host remains usable after rejecting an operation.
        ProviderClient.validate(&config).await?;
        assert_eq!(
            model_requests(&inputs.records()?, "provider.validate").len(),
            1
        );
        Ok(())
    })
    .await
}
