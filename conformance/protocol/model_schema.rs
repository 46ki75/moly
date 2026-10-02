//! Golden cross-language payloads guard the semantic schema independently of hosting.
use moly_protocol::model::{ModelMessage, ModelRequest, ModelStep, ProviderConfig};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error>;
const SCHEMA: &str = include_str!("../schemas/model-provider-v1.json");

#[test]
fn provider_payloads_roundtrip_without_upstream_encoding() -> Result<(), Error> {
    let request = json!({
        "options":{"arbitrary_implementation_option":true}, "credential":null,
        "context":{
            "session_id":"00000000-0000-4000-8000-000000000001",
            "run_id":"00000000-0000-4000-8000-000000000002",
            "model_call_id":"00000000-0000-4000-8000-000000000003", "call_kind":"primary"
        },
        "messages":[
            {"kind":"user","text":"synthetic input"},
            {"kind":"assistant","text":null,"tool_calls":[{"id":"call","name":"echo","arguments":{"n":1}}],"metadata":{"format":"fixture.v1","value":{"opaque":true}}},
            {"kind":"tool_result","call_id":"call","output":{"n":1}}
        ],
        "tools":[{"name":"echo","description":"Synthetic capability","input_schema":{"type":"object"}}]
    });
    let typed: ModelRequest = serde_json::from_value(request.clone())?;
    assert_eq!(serde_json::to_value(&typed)?, request);
    assert!(
        matches!(&typed.messages[2], ModelMessage::ToolResult { output, .. } if output.is_object())
    );
    for step in [
        json!({"outcome":"completed","text":"done","metadata":null}),
        json!({"outcome":"await_host_tools","text":null,"calls":[{"id":"call","name":"echo","arguments":{"n":1}}],"metadata":null}),
    ] {
        let typed: ModelStep = serde_json::from_value(step.clone())?;
        assert_eq!(serde_json::to_value(typed)?, step);
    }
    assert!(serde_json::from_value::<ModelStep>(json!({"outcome":"future_outcome"})).is_err());
    assert!(serde_json::from_value::<ModelStep>(json!({"outcome":"completed"})).is_err());
    let mut future_request = request;
    future_request["future_field"] = json!(true);
    assert!(serde_json::from_value::<ModelRequest>(future_request).is_ok());
    Ok(())
}

#[test]
fn language_neutral_schema_is_self_contained_and_names_all_payloads() -> Result<(), Error> {
    let schema: Value = serde_json::from_str(SCHEMA)?;
    assert_eq!(schema["$id"], "urn:moly:model-provider:1");
    for name in [
        "Envelope",
        "Initialize",
        "Initialized",
        "Command",
        "ProviderConfig",
        "OpenAIOptions",
        "Context",
        "Metadata",
        "ToolCall",
        "ToolDefinition",
        "ModelMessage",
        "ModelRequest",
        "ModelStep",
        "Error",
    ] {
        assert!(schema["$defs"][name].is_object(), "missing schema: {name}");
    }
    fn references(value: &Value, root: &Value) {
        match value {
            Value::Object(fields) => {
                if let Some(reference) = fields.get("$ref").and_then(Value::as_str) {
                    let pointer = reference
                        .strip_prefix('#')
                        .expect("schema must be self-contained");
                    assert!(
                        root.pointer(pointer).is_some(),
                        "unresolved schema reference: {reference}"
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
    // Optional command fields default identically in Rust and the published schema.
    let config: ProviderConfig = serde_json::from_value(
        json!({"command":{"executable":"/example/provider"}, "options":{}}),
    )?;
    assert!(config.command.args.is_empty());
    assert!(config.command.env.is_empty());
    assert_eq!(
        schema["$defs"]["Command"]["properties"]["args"]["default"],
        json!([])
    );
    assert_eq!(
        schema["$defs"]["Command"]["properties"]["env"]["default"],
        json!({})
    );
    Ok(())
}
