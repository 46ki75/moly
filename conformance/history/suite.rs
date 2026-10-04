//! History is a versioned data contract, not an MPP wire capture.
use moly_protocol::history::{ConversationHistory, HistoryEntry, HistoryError, HistoryRecord};
use serde_json::{Value, json};

#[path = "codec.rs"]
mod codec;
#[path = "golden.rs"]
mod golden;
#[path = "schema.rs"]
mod schema;
#[path = "semantics.rs"]
mod semantics;

const EMPTY: &str = include_str!("empty.jsonl");
const BRANCHED: &str = include_str!("branched.jsonl");
const SELECTED: &str = include_str!("selected-branch.jsonl");
const SCHEMA: &str = include_str!("../schemas/conversation-history-v1.json");
const MISSING_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const NIL_ID: &str = "00000000-0000-0000-0000-000000000000";

type Error = Box<dyn std::error::Error>;

#[test]
fn history_contract_is_present_and_independent_of_component_versions() -> Result<(), Error> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(
        root.join("schemas/conversation-history-v1.json"),
    )?)?;
    assert_eq!(schema["$id"], "urn:moly:conversation-history:1");
    assert_eq!(moly_protocol::VERSION, 1);
    assert_eq!(moly_protocol::SERVER_VERSION, 3);
    assert_eq!(moly_protocol::model::PROVIDER_VERSION, 2);
    assert!(root.join("history/branched.jsonl").is_file());
    Ok(())
}

fn records(text: &str) -> Result<Vec<Value>, Error> {
    text.lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn jsonl(records: &[Value]) -> Result<String, Error> {
    let mut text = String::new();
    for record in records {
        text.push_str(&serde_json::to_string(record)?);
        text.push('\n');
    }
    Ok(text)
}

fn entry_mut(records: &mut [Value], sequence: u64) -> &mut Value {
    records
        .iter_mut()
        .find(|record| record["type"] == "entry" && record["sequence"] == sequence)
        .expect("fixture contains the requested sequence")
}

fn entry_id(records: &[Value], sequence: u64) -> Value {
    records
        .iter()
        .find(|record| record["type"] == "entry" && record["sequence"] == sequence)
        .expect("fixture contains the requested sequence")["id"]
        .clone()
}

fn selection_mut(records: &mut [Value]) -> &mut Value {
    records.last_mut().expect("fixture has a selection")
}

fn assert_rejected(text: &str, label: &str) -> HistoryError {
    match ConversationHistory::from_jsonl(text) {
        Ok(_) => panic!("history accepted invalid case: {label}"),
        Err(error) => error,
    }
}

fn unchecked(records: &[Value]) -> Result<ConversationHistory, Error> {
    // Use only public DTO decoding so pure validation is tested independently of
    // from_jsonl's validation; no executable implementation is linked or included.
    let mut header = None;
    let mut entries = Vec::new();
    let mut selection = None;
    for record in records {
        match serde_json::from_value::<HistoryRecord>(record.clone())? {
            HistoryRecord::Session(value) => header = Some(value),
            HistoryRecord::Entry(value) => entries.push(value),
            HistoryRecord::Selection(value) => selection = Some(value),
        }
    }
    Ok(ConversationHistory {
        header: header.expect("semantic fixture has a session"),
        entries,
        selection: selection.expect("semantic fixture has a selection"),
    })
}

fn assert_invalid_history(history: &ConversationHistory, label: &str) {
    assert!(history.validate().is_err(), "validate accepted: {label}");
    assert!(
        history.selected_path().is_err(),
        "selected_path accepted: {label}"
    );
    assert!(history.to_jsonl().is_err(), "to_jsonl accepted: {label}");
}

fn assert_semantically_rejected(records: &[Value], label: &str) -> Result<(), Error> {
    assert_invalid_history(&unchecked(records)?, label);
    assert_rejected(&jsonl(records)?, label);
    Ok(())
}

fn typed_entry_mut(history: &mut ConversationHistory, sequence: u64) -> &mut HistoryEntry {
    history
        .entries
        .iter_mut()
        .find(|entry| entry.sequence == sequence)
        .expect("fixture contains the requested sequence")
}

fn path_sequences(history: &ConversationHistory) -> Result<Vec<u64>, Error> {
    Ok(history
        .selected_path()?
        .iter()
        .map(|entry| entry.sequence)
        .collect())
}

fn canonical_values(mut records: Vec<Value>) -> Vec<Value> {
    let selection = records.pop().expect("fixture has a selection");
    let header = records.remove(0);
    records.sort_by_key(|record| record["sequence"].as_u64().expect("entry sequence is u64"));
    records.insert(0, header);
    records.push(selection);
    // Optional nulls and omissions mean the same thing; the contract deliberately
    // does not mandate a serialization policy for these optional fields.
    for record in &mut records {
        let (object, keys): (&mut Value, &[&str]) = if record["type"] == "session" {
            (record, &["name", "workspace", "forked_from"])
        } else if record["data"]["kind"] == "assistant" {
            (&mut record["data"], &["metadata", "attribution"])
        } else {
            continue;
        };
        for key in keys {
            if object.get(key).is_some_and(Value::is_null) {
                object
                    .as_object_mut()
                    .expect("semantic object")
                    .remove(*key);
            }
        }
    }
    records
}

fn one_entry(data: Value) -> Vec<Value> {
    vec![
        json!({"type":"session", "format_version":1,
            "session_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "created_at_ms":0, "scope":"full_tree"}),
        json!({"type":"entry", "id":"00000000-0000-4000-8000-000000000001",
            "parent_id":null, "sequence":1, "timestamp_ms":0, "data":data}),
        json!({"type":"selection", "head":"00000000-0000-4000-8000-000000000001", "revision":1}),
    ]
}
