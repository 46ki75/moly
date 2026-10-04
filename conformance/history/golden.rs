//! Golden data preserves identities and opaque values, not live runtime state.
use super::*;
use moly_protocol::SessionId;
use moly_protocol::history::{EntryData, ExportScope, FORMAT_VERSION};

#[test]
fn golden_snapshots_restore_identity_and_roundtrip_without_loss() -> Result<(), Error> {
    assert_eq!(FORMAT_VERSION, 1);
    for fixture in [EMPTY, BRANCHED, SELECTED] {
        let history = ConversationHistory::from_jsonl(fixture)?;
        history.validate()?;
        let expected_id = records(fixture)?[0]["session_id"].clone();
        // The restored identity is the same shared SessionId used by MPP, not a
        // newly allocated session, connection, run, or upstream thread identity.
        let restored_id: SessionId = history.header.session_id;
        assert!(
            json!(restored_id) == expected_id,
            "restoration changed session ID"
        );
        let encoded = history.to_jsonl()?;
        assert!(encoded.ends_with('\n'));
        assert!(!encoded.contains('\r'));
        assert!(
            canonical_values(records(&encoded)?) == canonical_values(records(fixture)?),
            "golden roundtrip changed structured content"
        );
        let restored = ConversationHistory::from_jsonl(&encoded)?;
        assert!(restored.header.session_id == restored_id);
        assert_eq!(path_sequences(&restored)?, path_sequences(&history)?);
        assert!(
            restored.to_jsonl()? == encoded,
            "canonical output is not stable"
        );
    }
    Ok(())
}

#[test]
fn arbitrary_physical_entry_order_has_one_canonical_export() -> Result<(), Error> {
    let original = records(BRANCHED)?;
    let expected = ConversationHistory::from_jsonl(BRANCHED)?.to_jsonl()?;
    let count = original.len() - 2;
    for rotation in 0..count {
        for reverse in [false, true] {
            let mut entries = original[1..original.len() - 1].to_vec();
            entries.rotate_left(rotation);
            if reverse {
                entries.reverse();
            }
            let mut shuffled = vec![original[0].clone()];
            shuffled.extend(entries);
            shuffled.push(original.last().expect("selection exists").clone());
            let history = ConversationHistory::from_jsonl(&jsonl(&shuffled)?)?;
            assert!(
                history.to_jsonl()? == expected,
                "physical order affected export"
            );
            assert_eq!(path_sequences(&history)?, [1, 2, 4, 7, 9, 12, 16, 18, 20]);
        }
    }
    let exported = records(&expected)?;
    let sequences: Vec<_> = exported[1..exported.len() - 1]
        .iter()
        .map(|record| record["sequence"].as_u64().expect("entry sequence"))
        .collect();
    assert_eq!(sequences, [1, 2, 4, 7, 9, 10, 12, 14, 16, 18, 20, 22, 25]);
    Ok(())
}

#[test]
fn selected_path_excludes_abandoned_results_and_last_physical_provider_state() -> Result<(), Error>
{
    let history = ConversationHistory::from_jsonl(BRANCHED)?;
    assert_eq!(path_sequences(&history)?, [1, 2, 4, 7, 9, 12, 16, 18, 20]);
    let raw = records(BRANCHED)?;
    assert_eq!(raw[raw.len() - 2]["sequence"], 14);
    let path = history.selected_path()?;
    let mut states = Vec::new();
    let mut results = Vec::new();
    for entry in &path {
        match &entry.data {
            EntryData::ProviderState { scope, state } => {
                assert_eq!(scope.provider, "fixture");
                assert_eq!(scope.binding, "account-a/config-1");
                states.push((entry.sequence, serde_json::to_value(state)?));
            }
            EntryData::ToolResult {
                assistant_id,
                call_id,
                output,
            } => {
                assert!(json!(assistant_id) == entry_id(&raw, 7));
                assert_eq!(call_id, "call-a");
                assert!(output == &json!({"branch":"selected", "n":u64::MAX}));
                results.push(entry.sequence);
            }
            _ => {}
        }
    }
    assert_eq!(results, [9]);
    assert_eq!(
        states
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        [12, 16]
    );
    assert_eq!(states[0].1["value"]["thread"], "selected-branch");
    assert!(states[1].1.is_null(), "explicit clear was lost");
    assert!(history.entries.iter().any(|entry| entry.sequence == 14));
    // This examines stored ancestry only. It does not infer an active Provider
    // session, project model context, authorize replay, or validate upstream data.
    Ok(())
}

#[test]
fn changing_head_or_export_scope_never_mints_identity_or_drops_other_branches() -> Result<(), Error>
{
    let mut history = ConversationHistory::from_jsonl(BRANCHED)?;
    let identity = history.header.session_id;
    let count = history.entries.len();
    let abandoned = history
        .entries
        .iter()
        .find(|entry| entry.sequence == 14)
        .expect("branch head")
        .id;
    history.selection.head = Some(abandoned);
    history.selection.revision += 1;
    assert_eq!(path_sequences(&history)?, [1, 2, 4, 7, 10, 14]);
    let restored = ConversationHistory::from_jsonl(&history.to_jsonl()?)?;
    assert!(restored.header.session_id == identity);
    assert_eq!(restored.entries.len(), count);
    history.selection.head = None;
    history.validate()?;
    assert!(history.selected_path()?.is_empty());
    assert_eq!(history.entries.len(), count);
    let mut selected = history.clone();
    selected.header.scope = ExportScope::SelectedBranch;
    assert_invalid_history(&selected, "nonempty selected_branch with virtual-root head");
    selected.entries.clear();
    selected.validate()?;
    assert!(selected.selected_path()?.is_empty());
    assert!(
        ConversationHistory::from_jsonl(&selected.to_jsonl()?)?
            .header
            .session_id
            == identity
    );
    Ok(())
}

#[test]
fn selected_branch_fork_provenance_is_external_and_preserved() -> Result<(), Error> {
    let mut fork = ConversationHistory::from_jsonl(SELECTED)?;
    assert!(matches!(fork.header.scope, ExportScope::SelectedBranch));
    assert_eq!(path_sequences(&fork)?, [4, 7, 18]);
    let origin = fork
        .header
        .forked_from
        .as_ref()
        .expect("fixture has fork origin");
    assert!(origin.session_id != fork.header.session_id);
    assert!(
        fork.entries
            .iter()
            .all(|entry| Some(entry.id) != origin.entry_id)
    );
    assert!(fork.selection.revision == u64::MAX);
    let snapshot = fork.to_jsonl()?;
    let restored = ConversationHistory::from_jsonl(&snapshot)?;
    let restored_origin = restored
        .header
        .forked_from
        .as_ref()
        .expect("restored fork origin");
    assert!(restored_origin.session_id == origin.session_id);
    assert!(restored_origin.entry_id == origin.entry_id);
    fork.header
        .forked_from
        .as_mut()
        .expect("fork origin")
        .entry_id = None;
    fork.validate()?;
    assert!(records(&fork.to_jsonl()?)?[0]["forked_from"]["entry_id"].is_null());
    fork.header.scope = ExportScope::FullTree;
    fork.validate()?;
    assert_eq!(path_sequences(&fork)?, [4, 7, 18]);
    Ok(())
}

#[test]
fn opaque_values_preserve_all_json_shapes_and_u64_precision() -> Result<(), Error> {
    for opaque in [
        Value::Null,
        json!(false),
        json!(u64::MAX),
        json!(-1),
        json!(1.25),
        json!("synthetic"),
        json!([null, true, u64::MAX]),
        json!({"type":"future", "kind":"opaque", "extensions":{"unknown":u64::MAX}}),
    ] {
        for data in [
            json!({"kind":"assistant", "text":"", "tool_calls":[],
                "metadata":{"format":"opaque.v9", "value":opaque}, "attribution":null}),
            json!({"kind":"provider_state", "scope":{"provider":"synthetic", "binding":"config"},
                "state":{"format":"opaque.v9", "value":opaque}}),
        ] {
            let raw = one_entry(data);
            let history = ConversationHistory::from_jsonl(&jsonl(&raw)?)?;
            assert!(canonical_values(records(&history.to_jsonl()?)?) == canonical_values(raw));
        }
        let mut raw = records(BRANCHED)?;
        entry_mut(&mut raw, 9)["data"]["output"] = opaque;
        let history = ConversationHistory::from_jsonl(&jsonl(&raw)?)?;
        assert!(canonical_values(records(&history.to_jsonl()?)?) == canonical_values(raw));
    }
    let mut raw = one_entry(json!({"kind":"user", "text":""}));
    raw[0]["created_at_ms"] = json!(u64::MAX);
    raw[1]["sequence"] = json!(u64::MAX);
    raw[1]["timestamp_ms"] = json!(u64::MAX);
    raw[2]["revision"] = json!(u64::MAX);
    let history = ConversationHistory::from_jsonl(&jsonl(&raw)?)?;
    assert_eq!(history.header.created_at_ms, u64::MAX);
    assert_eq!(history.entries[0].sequence, u64::MAX);
    assert_eq!(history.entries[0].timestamp_ms, u64::MAX);
    assert_eq!(history.selection.revision, u64::MAX);
    assert!(canonical_values(records(&history.to_jsonl()?)?) == canonical_values(raw));
    Ok(())
}

#[test]
fn history_dtos_have_no_mpp_batch_or_tool_name_policy() -> Result<(), Error> {
    let calls: Vec<_> = (0..40)
        .map(|index| json!({"id":format!("call-{index}"), "name":"arbitrary tool name / ☃", "arguments":{}}))
        .collect();
    let raw = one_entry(json!({"kind":"assistant", "text":null, "tool_calls":calls}));
    let history = ConversationHistory::from_jsonl(&jsonl(&raw)?)?;
    if let EntryData::Assistant { tool_calls, .. } = &history.entries[0].data {
        let _: &moly_protocol::history::ToolCall = &tool_calls[0];
        assert_eq!(tool_calls.len(), 40);
    } else {
        panic!("expected history-owned assistant DTO");
    }
    let raw = one_entry(json!({"kind":"tools", "tools":[{
        "name":"arbitrary tool name / ☃", "description":"", "input_schema":{}}]}));
    let history = ConversationHistory::from_jsonl(&jsonl(&raw)?)?;
    if let EntryData::Tools { tools } = &history.entries[0].data {
        let _: &moly_protocol::history::ToolDefinition = &tools[0];
    } else {
        panic!("expected history-owned tools DTO");
    }
    Ok(())
}
