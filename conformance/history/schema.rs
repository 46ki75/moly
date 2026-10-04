//! Structural schema checks deliberately do not implement a JSON Schema engine.
use super::*;
use std::collections::BTreeSet;

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
            for child in fields.values() {
                references(child, root);
            }
        }
        Value::Array(values) => {
            for child in values {
                references(child, root);
            }
        }
        _ => {}
    }
}

fn string_set(value: &Value) -> BTreeSet<&str> {
    value
        .as_array()
        .expect("schema string array")
        .iter()
        .map(|item| item.as_str().expect("schema string"))
        .collect()
}

#[test]
fn schema_is_self_contained_draft_2020_12_for_individual_records() -> Result<(), Error> {
    let schema: Value = serde_json::from_str(SCHEMA)?;
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["$id"], "urn:moly:conversation-history:1");
    assert!(schema.get("$ref").is_none());
    assert!(
        schema["oneOf"]
            == json!([
                {"$ref":"#/$defs/SessionRecord"}, {"$ref":"#/$defs/EntryRecord"},
                {"$ref":"#/$defs/SelectionRecord"}
            ])
    );
    references(&schema, &schema);
    for (definition, discriminator) in [
        ("SessionRecord", "session"),
        ("EntryRecord", "entry"),
        ("SelectionRecord", "selection"),
    ] {
        assert_eq!(
            schema["$defs"][definition]["properties"]["type"]["const"],
            discriminator
        );
    }
    let alternatives: BTreeSet<_> = schema["$defs"]["EntryData"]["oneOf"]
        .as_array()
        .expect("entry data alternatives")
        .iter()
        .map(|item| item["$ref"].as_str().expect("local data reference"))
        .collect();
    assert_eq!(
        alternatives,
        [
            "#/$defs/User",
            "#/$defs/Assistant",
            "#/$defs/ToolResult",
            "#/$defs/SystemPrompt",
            "#/$defs/Tools",
            "#/$defs/ProviderState",
            "#/$defs/SessionMetadata",
        ]
        .into_iter()
        .collect()
    );
    // Fixtures are checked by the public Rust codec, not a third-party schema
    // validator. Resolving references alone does not prove full schema validity.
    for fixture in [EMPTY, BRANCHED, SELECTED] {
        let history = ConversationHistory::from_jsonl(fixture)?;
        history.validate()?;
        for record in records(fixture)? {
            let typed: HistoryRecord = serde_json::from_value(record)?;
            assert!(serde_json::to_value(typed)?.is_object());
        }
    }
    Ok(())
}

#[test]
fn schema_exact_semantic_objects_match_required_and_optional_contract_fields() -> Result<(), Error>
{
    let schema: Value = serde_json::from_str(SCHEMA)?;
    for (name, required, optional) in [
        (
            "SessionRecord",
            &[
                "type",
                "format_version",
                "session_id",
                "created_at_ms",
                "scope",
            ][..],
            &["name", "workspace", "forked_from"][..],
        ),
        (
            "EntryRecord",
            &[
                "type",
                "id",
                "parent_id",
                "sequence",
                "timestamp_ms",
                "data",
            ][..],
            &[][..],
        ),
        (
            "SelectionRecord",
            &["type", "head", "revision"][..],
            &[][..],
        ),
        ("ForkOrigin", &["session_id", "entry_id"][..], &[][..]),
        ("ProviderData", &["format", "value"][..], &[][..]),
        ("ProviderScope", &["provider", "binding"][..], &[][..]),
        (
            "Attribution",
            &["provider", "binding", "model"][..],
            &[][..],
        ),
        ("ToolCall", &["id", "name", "arguments"][..], &[][..]),
        (
            "ToolDefinition",
            &["name", "description", "input_schema"][..],
            &[][..],
        ),
        ("User", &["kind", "text"][..], &[][..]),
        (
            "Assistant",
            &["kind", "text", "tool_calls"][..],
            &["metadata", "attribution"][..],
        ),
        (
            "ToolResult",
            &["kind", "assistant_id", "call_id", "output"][..],
            &[][..],
        ),
        ("SystemPrompt", &["kind", "text"][..], &[][..]),
        ("Tools", &["kind", "tools"][..], &[][..]),
        ("ProviderState", &["kind", "scope", "state"][..], &[][..]),
        ("SessionMetadata", &["kind", "name"][..], &[][..]),
    ] {
        let definition = &schema["$defs"][name];
        assert_eq!(
            definition["type"], "object",
            "semantic DTO must be object-only: {name}"
        );
        assert_eq!(
            definition["additionalProperties"], false,
            "unknown fields must fail: {name}"
        );
        assert_eq!(
            string_set(&definition["required"]),
            required.iter().copied().collect(),
            "required fields: {name}"
        );
        let fields: BTreeSet<_> = definition["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            fields,
            required.iter().chain(optional).copied().collect(),
            "exact fields: {name}"
        );
    }
    for (name, kind) in [
        ("User", "user"),
        ("Assistant", "assistant"),
        ("ToolResult", "tool_result"),
        ("SystemPrompt", "system_prompt"),
        ("Tools", "tools"),
        ("ProviderState", "provider_state"),
        ("SessionMetadata", "session_metadata"),
    ] {
        assert_eq!(schema["$defs"][name]["properties"]["kind"]["const"], kind);
    }
    Ok(())
}

#[test]
fn schema_preserves_nullability_opaque_shapes_and_integer_precision() -> Result<(), Error> {
    let schema: Value = serde_json::from_str(SCHEMA)?;
    let defs = &schema["$defs"];
    for name in ["U64", "Sequence"] {
        assert_eq!(defs[name]["type"], "integer");
        assert_eq!(defs[name]["maximum"].as_u64(), Some(u64::MAX));
    }
    assert_eq!(defs["U64"]["minimum"], 0);
    assert_eq!(defs["Sequence"]["minimum"], 1);
    assert_eq!(
        defs["SessionRecord"]["properties"]["format_version"]["const"],
        1
    );
    assert!(
        defs["SessionRecord"]["properties"]["scope"]["enum"]
            == json!(["full_tree", "selected_branch"])
    );
    assert_eq!(defs["Identity"]["type"], "string");
    assert_eq!(defs["Identity"]["format"], "uuid");
    assert_eq!(
        defs["Identity"]["pattern"],
        "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    );
    assert_eq!(defs["Identity"]["not"]["const"], NIL_ID);
    for (name, field, target) in [
        ("SessionRecord", "session_id", "Identity"),
        ("ForkOrigin", "session_id", "Identity"),
        ("ForkOrigin", "entry_id", "NullableIdentity"),
        ("EntryRecord", "id", "Identity"),
        ("EntryRecord", "parent_id", "NullableIdentity"),
        ("SelectionRecord", "head", "NullableIdentity"),
        ("ToolResult", "assistant_id", "Identity"),
        ("SessionRecord", "created_at_ms", "U64"),
        ("EntryRecord", "sequence", "Sequence"),
        ("EntryRecord", "timestamp_ms", "U64"),
        ("SelectionRecord", "revision", "U64"),
    ] {
        assert_eq!(
            defs[name]["properties"][field]["$ref"],
            format!("#/$defs/{target}")
        );
    }
    assert!(
        defs["NullableIdentity"]["oneOf"] == json!([{"$ref":"#/$defs/Identity"}, {"type":"null"}])
    );
    for (name, field) in [
        ("Assistant", "text"),
        ("SessionMetadata", "name"),
        ("SessionRecord", "name"),
        ("SessionRecord", "workspace"),
    ] {
        assert!(defs[name]["properties"][field]["type"] == json!(["string", "null"]));
    }
    for (name, field, target) in [
        ("Assistant", "metadata", "ProviderData"),
        ("Assistant", "attribution", "Attribution"),
        ("ProviderState", "state", "ProviderData"),
        ("SessionRecord", "forked_from", "ForkOrigin"),
    ] {
        assert!(
            defs[name]["properties"][field]["oneOf"]
                == json!([
                    {"$ref":format!("#/$defs/{target}")}, {"type":"null"}
                ])
        );
    }
    assert!(defs["ProviderData"]["properties"]["value"] == json!({}));
    assert!(defs["ToolResult"]["properties"]["output"] == json!({}));
    for (name, field) in [
        ("ToolCall", "arguments"),
        ("ToolDefinition", "input_schema"),
    ] {
        assert!(defs[name]["properties"][field] == json!({"type":"object"}));
    }
    for (name, field) in [
        ("ProviderData", "format"),
        ("ProviderScope", "provider"),
        ("ProviderScope", "binding"),
        ("Attribution", "provider"),
        ("Attribution", "binding"),
        ("Attribution", "model"),
        ("ToolCall", "id"),
        ("ToolCall", "name"),
        ("ToolResult", "call_id"),
        ("ToolDefinition", "name"),
    ] {
        assert_eq!(defs[name]["properties"][field]["$ref"], "#/$defs/Nonblank");
    }
    assert_eq!(defs["Nonblank"]["type"], "string");
    assert!(defs["Nonblank"]["pattern"].is_string());
    assert!(
        defs["Assistant"]["anyOf"]
            == json!([
                {"properties":{"text":{"type":"string"}}},
                {"properties":{"tool_calls":{"minItems":1}}}
            ])
    );
    assert!(
        defs["Assistant"]["properties"]["tool_calls"]
            .get("maxItems")
            .is_none()
    );
    Ok(())
}
