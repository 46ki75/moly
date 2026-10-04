//! Mutation checks exercise both strict parsing and public pure validation.
use super::*;
use moly_protocol::SessionId;
use moly_protocol::history::{EntryData, EntryId, ExportScope};
use uuid::Uuid;

#[test]
fn graph_scope_order_revision_and_provenance_mutations_fail_closed() -> Result<(), Error> {
    for label in [
        "duplicate entry ID",
        "duplicate sequence",
        "zero sequence",
        "missing parent",
        "self parent",
        "parent after child",
        "cycle",
        "missing head",
        "stale revision",
        "revision misses abandoned branch",
        "selected scope with other branches",
        "selected scope with null head",
        "self fork",
        "orphaned selected ancestry",
    ] {
        let mut raw = records(BRANCHED)?;
        match label {
            "duplicate entry ID" => {
                let duplicate = entry_id(&raw, 1);
                entry_mut(&mut raw, 2)["id"] = duplicate;
            }
            "duplicate sequence" => entry_mut(&mut raw, 2)["sequence"] = json!(1),
            "zero sequence" => entry_mut(&mut raw, 1)["sequence"] = json!(0),
            "missing parent" => entry_mut(&mut raw, 4)["parent_id"] = json!(MISSING_ID),
            "self parent" => {
                let id = entry_id(&raw, 4);
                entry_mut(&mut raw, 4)["parent_id"] = id;
            }
            "parent after child" => {
                let id = entry_id(&raw, 7);
                entry_mut(&mut raw, 4)["parent_id"] = id;
            }
            "cycle" => {
                let id = entry_id(&raw, 4);
                entry_mut(&mut raw, 1)["parent_id"] = id;
            }
            "missing head" => selection_mut(&mut raw)["head"] = json!(MISSING_ID),
            "stale revision" => selection_mut(&mut raw)["revision"] = json!(0),
            "revision misses abandoned branch" => selection_mut(&mut raw)["revision"] = json!(20),
            "selected scope with other branches" => raw[0]["scope"] = json!("selected_branch"),
            "selected scope with null head" => {
                raw[0]["scope"] = json!("selected_branch");
                selection_mut(&mut raw)["head"] = Value::Null;
            }
            "self fork" => {
                raw[0]["forked_from"] = json!({"session_id":raw[0]["session_id"], "entry_id":null});
            }
            "orphaned selected ancestry" => {
                raw = records(SELECTED)?;
                raw.retain(|record| record["type"] != "entry" || record["sequence"] != 7);
            }
            _ => unreachable!("fixed mutation table"),
        }
        assert_semantically_rejected(&raw, label)?;
    }
    Ok(())
}

#[test]
fn tool_results_require_ancestor_assistant_call_and_branch_unique_pair() -> Result<(), Error> {
    for label in [
        "missing assistant",
        "nonassistant reference",
        "descendant assistant",
        "assistant on sibling branch",
        "unadvertised call",
        "blank result call",
        "duplicate result on abandoned branch",
        "duplicate result on selected branch",
        "duplicate assistant call IDs",
        "empty assistant",
    ] {
        let mut raw = records(BRANCHED)?;
        match label {
            "missing assistant" => {
                entry_mut(&mut raw, 9)["data"]["assistant_id"] = json!(MISSING_ID)
            }
            "nonassistant reference" => {
                let id = entry_id(&raw, 4);
                entry_mut(&mut raw, 9)["data"]["assistant_id"] = id;
            }
            "descendant assistant" => {
                let id = entry_id(&raw, 20);
                entry_mut(&mut raw, 9)["data"]["assistant_id"] = id;
            }
            "assistant on sibling branch" => {
                // Make a valid, earlier assistant on a separate root branch. Its
                // advertised call still cannot justify a result outside its ancestry.
                entry_mut(&mut raw, 25)["sequence"] = json!(6);
                entry_mut(&mut raw, 6)["data"] = json!({"kind":"assistant", "text":null,
                    "tool_calls":[{"id":"call-a", "name":"echo", "arguments":{}}]});
                let id = entry_id(&raw, 6);
                entry_mut(&mut raw, 9)["data"]["assistant_id"] = id;
            }
            "unadvertised call" => {
                entry_mut(&mut raw, 9)["data"]["call_id"] = json!("missing-call")
            }
            "blank result call" => entry_mut(&mut raw, 9)["data"]["call_id"] = json!(""),
            "duplicate result on abandoned branch" => {
                let id = entry_id(&raw, 9);
                entry_mut(&mut raw, 10)["parent_id"] = id;
            }
            "duplicate result on selected branch" => {
                let id = entry_id(&raw, 10);
                entry_mut(&mut raw, 9)["parent_id"] = id;
                // Keep the graph independently well-ordered to isolate branch
                // duplicate-result validation from the sequence check.
                entry_mut(&mut raw, 9)["sequence"] = json!(11);
            }
            "duplicate assistant call IDs" => {
                entry_mut(&mut raw, 7)["data"]["tool_calls"][1]["id"] = json!("call-a");
            }
            "empty assistant" => {
                entry_mut(&mut raw, 20)["data"]["text"] = Value::Null;
            }
            _ => unreachable!("fixed mutation table"),
        }
        assert_semantically_rejected(&raw, label)?;
    }
    Ok(())
}

#[test]
fn siblings_and_incomplete_tool_batches_remain_valid() -> Result<(), Error> {
    let raw = records(BRANCHED)?;
    let history = ConversationHistory::from_jsonl(BRANCHED)?;
    let results: Vec<_> = history
        .entries
        .iter()
        .filter_map(|entry| {
            if let EntryData::ToolResult { call_id, .. } = &entry.data {
                Some((entry.sequence, call_id.as_str()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(_, call)| *call == "call-a"));
    let mut incomplete = raw.clone();
    incomplete.retain(|record| {
        record["type"] != "entry"
            || record["sequence"]
                .as_u64()
                .is_some_and(|sequence| sequence <= 7)
    });
    selection_mut(&mut incomplete)["head"] = entry_id(&raw, 7);
    let history = ConversationHistory::from_jsonl(&jsonl(&incomplete)?)?;
    history.validate()?;
    assert_eq!(path_sequences(&history)?, [1, 2, 4, 7]);
    // Call IDs are assistant-local, not snapshot-global. Two assistants may
    // advertise call-a and have results for it on the same path.
    let mut repeated = raw.clone();
    entry_mut(&mut repeated, 20)["data"] = json!({"kind":"assistant", "text":null,
        "tool_calls":[{"id":"call-a", "name":"echo", "arguments":{}}]});
    let mut result = entry_mut(&mut repeated, 9).clone();
    result["id"] = json!(MISSING_ID);
    result["parent_id"] = entry_id(&repeated, 20);
    result["sequence"] = json!(26);
    result["data"]["assistant_id"] = entry_id(&repeated, 20);
    let index = repeated.len() - 1;
    repeated.insert(index, result);
    selection_mut(&mut repeated)["head"] = json!(MISSING_ID);
    ConversationHistory::from_jsonl(&jsonl(&repeated)?)?.validate()?;
    Ok(())
}

#[test]
fn required_nonblank_nested_fields_and_unique_tool_names_are_semantic() -> Result<(), Error> {
    let paths = [
        (7, "/data/tool_calls/0/id"),
        (7, "/data/tool_calls/0/name"),
        (7, "/data/metadata/format"),
        (7, "/data/attribution/provider"),
        (7, "/data/attribution/binding"),
        (7, "/data/attribution/model"),
        (2, "/data/tools/0/name"),
        (12, "/data/scope/provider"),
        (12, "/data/scope/binding"),
        (12, "/data/state/format"),
    ];
    for (sequence, pointer) in paths {
        for blank in [
            "",
            " \t\r\n",
            "\u{0085}\u{00a0}\u{1680}\u{2003}\u{202f}\u{3000}",
        ] {
            let mut raw = records(BRANCHED)?;
            *entry_mut(&mut raw, sequence)
                .pointer_mut(pointer)
                .expect("semantic fixture field") = json!(blank);
            assert_semantically_rejected(&raw, pointer)?;
        }
    }
    let mut raw = records(BRANCHED)?;
    let duplicate = entry_mut(&mut raw, 2)["data"]["tools"][0].clone();
    entry_mut(&mut raw, 2)["data"]["tools"]
        .as_array_mut()
        .expect("tools array")
        .push(duplicate);
    assert_semantically_rejected(&raw, "duplicate tool definition names")?;
    Ok(())
}

#[test]
fn directly_constructed_public_values_cannot_bypass_validation() -> Result<(), Error> {
    let original = ConversationHistory::from_jsonl(BRANCHED)?;
    for label in [
        "nil session",
        "nil entry",
        "nil parent",
        "nil head",
        "nil fork session",
        "nil fork entry",
        "nil tool assistant",
        "unsupported in-memory version",
        "nonobject arguments",
        "nonobject input schema",
        "selected scope cannot auto-filter",
    ] {
        let mut history = original.clone();
        match label {
            "nil session" => history.header.session_id = SessionId(Uuid::nil()),
            "nil entry" => typed_entry_mut(&mut history, 25).id = EntryId(Uuid::nil()),
            "nil parent" => {
                typed_entry_mut(&mut history, 25).parent_id = Some(EntryId(Uuid::nil()))
            }
            "nil head" => history.selection.head = Some(EntryId(Uuid::nil())),
            "nil fork session" | "nil fork entry" => {
                history.header.forked_from = ConversationHistory::from_jsonl(SELECTED)?
                    .header
                    .forked_from;
                let origin = history.header.forked_from.as_mut().expect("fork origin");
                if label == "nil fork session" {
                    origin.session_id = SessionId(Uuid::nil());
                } else {
                    origin.entry_id = Some(EntryId(Uuid::nil()));
                }
            }
            "nil tool assistant" => {
                if let EntryData::ToolResult { assistant_id, .. } =
                    &mut typed_entry_mut(&mut history, 9).data
                {
                    *assistant_id = EntryId(Uuid::nil());
                } else {
                    panic!("expected tool result");
                }
            }
            "unsupported in-memory version" => history.header.format_version = 2,
            "nonobject arguments" => {
                if let EntryData::Assistant { tool_calls, .. } =
                    &mut typed_entry_mut(&mut history, 7).data
                {
                    tool_calls[0].arguments = json!([]);
                } else {
                    panic!("expected assistant");
                }
            }
            "nonobject input schema" => {
                if let EntryData::Tools { tools } = &mut typed_entry_mut(&mut history, 2).data {
                    tools[0].input_schema = Value::Null;
                } else {
                    panic!("expected tools");
                }
            }
            "selected scope cannot auto-filter" => {
                history.header.scope = ExportScope::SelectedBranch
            }
            _ => unreachable!("fixed mutation table"),
        }
        assert_invalid_history(&history, label);
    }
    Ok(())
}

#[test]
fn selected_branch_scope_requires_exact_ancestry_without_rewriting_parents() -> Result<(), Error> {
    let original = ConversationHistory::from_jsonl(BRANCHED)?;
    let mut branch = original.clone();
    let path: Vec<_> = original
        .selected_path()?
        .iter()
        .map(|entry| entry.id)
        .collect();
    branch.entries.retain(|entry| path.contains(&entry.id));
    branch.header.scope = ExportScope::SelectedBranch;
    branch.validate()?;
    assert_eq!(branch.entries.len(), path.len());
    assert_eq!(path_sequences(&branch)?, path_sequences(&original)?);
    assert!(branch.header.session_id == original.header.session_id);
    for entry in &branch.entries {
        let source = original
            .entries
            .iter()
            .find(|source| source.id == entry.id)
            .expect("original entry");
        assert!(entry.parent_id == source.parent_id);
    }
    branch.selection.head = None;
    assert_invalid_history(&branch, "selected ancestry with virtual-root head");
    branch.entries.clear();
    branch.validate()?;
    Ok(())
}
