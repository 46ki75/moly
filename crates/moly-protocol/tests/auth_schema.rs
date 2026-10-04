//! Cross-language authentication payloads stay independent of executable internals.
use moly_protocol::{SERVER_VERSION, auth::*, model::PROVIDER_VERSION};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn roundtrip<T: Serialize + DeserializeOwned>(
    value: Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let typed: T = serde_json::from_value(value.clone())?;
    assert_eq!(serde_json::to_value(typed)?, value);
    Ok(())
}

#[test]
fn authentication_enums_require_strings_not_externally_tagged_objects() {
    for operation in ["login", "status", "logout"] {
        assert!(serde_json::from_value::<AuthOperation>(json!(operation)).is_ok());
        assert!(serde_json::from_value::<AuthOperation>(json!({operation:null})).is_err());
    }
    for outcome in ["opened", "declined", "unavailable"] {
        assert!(serde_json::from_value::<InteractionOutcome>(json!(outcome)).is_ok());
        assert!(serde_json::from_value::<InteractionOutcome>(json!({outcome:null})).is_err());
    }
}

#[test]
fn authentication_payloads_are_named_and_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let id = "00000000-0000-4000-8000-000000000001";
    roundtrip::<AuthCommand>(json!({"attempt_id":id,"operation":"login","config_revision":7}))?;
    roundtrip::<AuthCancel>(json!({"attempt_id":id}))?;
    roundtrip::<ProviderAuthRequest>(
        json!({"attempt_id":id,"operation":"status","options":{"opaque":true},"credential":null}),
    )?;
    roundtrip::<AuthStatus>(
        json!({"attempt_id":id,"authenticated":true,"registration":{"implementation_defined":true},"revocation_confirmed":null}),
    )?;
    roundtrip::<InteractionRequest>(
        json!({"attempt_id":id,"url":"https://example.invalid/authorize"}),
    )?;
    for outcome in ["opened", "declined", "unavailable"] {
        roundtrip::<InteractionResponse>(json!({"attempt_id":id,"outcome":outcome}))?;
    }
    roundtrip::<CredentialReplace>(json!({"credential":null}))?;
    roundtrip::<CredentialReplace>(json!({"credential":"synthetic-private-record"}))?;
    assert!(serde_json::from_value::<AuthOperation>(json!("future_auth_method")).is_err());
    assert!(
        serde_json::from_value::<InteractionResponse>(
            json!({"attempt_id":id,"outcome":"authenticated"})
        )
        .is_err()
    );
    let schema: Value = serde_json::from_str(include_str!(
        "../../../conformance/schemas/model-provider-v2.json"
    ))?;
    assert_eq!(schema["$id"], "urn:moly:model-provider:2");
    assert_eq!(
        schema["$defs"]["Initialize"]["properties"]["protocol_version"]["const"],
        PROVIDER_VERSION
    );
    assert_eq!(SERVER_VERSION, 3);
    for name in [
        "AuthCommand",
        "AuthCancel",
        "ProviderAuthRequest",
        "AuthStatus",
        "InteractionRequest",
        "InteractionResponse",
        "CredentialReplace",
    ] {
        assert_eq!(schema["$defs"][name]["type"], "object", "{name}");
    }
    fn references(value: &Value, root: &Value) {
        match value {
            Value::Object(fields) => {
                if let Some(reference) = fields.get("$ref").and_then(Value::as_str) {
                    assert!(
                        root.pointer(reference.strip_prefix('#').expect("self-contained schema"))
                            .is_some(),
                        "{reference}"
                    );
                }
                for value in fields.values() {
                    references(value, root);
                }
            }
            Value::Array(values) => {
                for value in values {
                    references(value, root);
                }
            }
            _ => {}
        }
    }
    references(&schema, &schema);
    Ok(())
}
