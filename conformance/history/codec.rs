//! Codec negatives are hermetic strings, not storage or network tests.
use super::*;
use moly_protocol::history::{
    EntryData, EntryId, MAX_ENTRIES, MAX_RECORD_BYTES, MAX_SNAPSHOT_BYTES,
};
use uuid::Uuid;

#[test]
fn record_order_delimiters_and_truncation_are_strict() -> Result<(), Error> {
    let raw = records(BRANCHED)?;
    let session = raw[0].clone();
    let selection = raw.last().expect("selection").clone();
    let entry = raw[1].clone();
    for (label, malformed) in [
        ("empty input", String::new()),
        ("blank input", "\n".into()),
        ("missing final LF", BRANCHED.trim_end_matches('\n').into()),
        (
            "partial final record",
            format!("{BRANCHED}{{\"type\":\"entry\""),
        ),
        (
            "malformed JSON",
            format!("{}\n{{\n", serde_json::to_string(&session)?),
        ),
        ("blank middle line", BRANCHED.replacen('\n', "\n\n", 1)),
        ("blank final line", format!("{BRANCHED}\n")),
        ("trailing record", format!("{BRANCHED}{{}}\n")),
        ("trailing non-JSON", format!("{BRANCHED}not-json\n")),
        ("UTF-8 BOM", format!("\u{feff}{EMPTY}")),
        ("CR without LF", EMPTY.replace('\n', "\r")),
        (
            "two JSON objects on one line",
            format!(
                "{}{}\n",
                serde_json::to_string(&session)?,
                serde_json::to_string(&selection)?
            ),
        ),
        ("session only", jsonl(std::slice::from_ref(&session))?),
        (
            "entry first",
            jsonl(&[entry.clone(), session.clone(), selection.clone()])?,
        ),
        ("selection only", jsonl(std::slice::from_ref(&selection))?),
        (
            "duplicate session",
            jsonl(&[session.clone(), session.clone(), selection.clone()])?,
        ),
        (
            "duplicate selection",
            jsonl(&[session.clone(), selection.clone(), selection.clone()])?,
        ),
        (
            "entry after selection",
            jsonl(&[session.clone(), selection.clone(), entry.clone()])?,
        ),
        ("entry without selection", jsonl(&[session.clone(), entry])?),
        ("record scalar", "null\n".into()),
        ("record array", "[]\n".into()),
    ] {
        assert_rejected(&malformed, label);
    }
    assert_eq!(
        assert_rejected("", "incomplete empty snapshot"),
        HistoryError::IncompleteSnapshot
    );
    let crlf = BRANCHED.replace('\n', "\r\n");
    let history = ConversationHistory::from_jsonl(&crlf)?;
    assert!(history.to_jsonl()? == ConversationHistory::from_jsonl(BRANCHED)?.to_jsonl()?);
    let whitespace = EMPTY
        .lines()
        .map(|line| format!(" \t{line} \t\r\n"))
        .collect::<String>();
    ConversationHistory::from_jsonl(&whitespace)?.validate()?;
    Ok(())
}

#[test]
fn unknown_versions_kinds_and_envelope_fields_do_not_migrate() -> Result<(), Error> {
    for version in [0, 2, 3, u16::MAX] {
        let mut raw = records(EMPTY)?;
        raw[0]["format_version"] = json!(version);
        assert_eq!(
            assert_rejected(&jsonl(&raw)?, "unsupported format version"),
            HistoryError::UnsupportedVersion
        );
    }
    for kind in [
        "future_kind",
        "attachment",
        "compaction",
        "run_started",
        "usage",
        "termination",
    ] {
        let raw = one_entry(json!({"kind":kind, "text":"synthetic"}));
        assert_rejected(&jsonl(&raw)?, "unknown entry kind");
    }
    for record_type in ["request", "event", "future_record", "Session", ""] {
        let mut raw = records(EMPTY)?;
        raw[0]["type"] = json!(record_type);
        assert_rejected(&jsonl(&raw)?, "unknown record type");
    }
    for scope in ["all", "branch", "FullTree", ""] {
        let mut raw = records(EMPTY)?;
        raw[0]["scope"] = json!(scope);
        assert_rejected(&jsonl(&raw)?, "unknown export scope");
    }
    for field in [
        "version",
        "protocol_version",
        "provider_version",
        "params",
        "header",
    ] {
        let mut raw = records(EMPTY)?;
        raw[0][field] = json!(1);
        assert_rejected(&jsonl(&raw)?, "history is not an envelope");
    }
    Ok(())
}

#[test]
fn unknown_fields_are_denied_in_every_semantic_object() -> Result<(), Error> {
    let base = records(SELECTED)?;
    for (sequence, pointer) in [
        (0, ""),
        (1, ""),
        (1, "/data"),
        (7, "/data/tool_calls/0"),
        (7, "/data/metadata"),
        (7, "/data/attribution"),
        (2, "/data/tools/0"),
        (12, "/data/scope"),
        (12, "/data/state"),
    ] {
        let mut raw = records(BRANCHED)?;
        let record = if sequence == 0 {
            &mut raw[0]
        } else {
            entry_mut(&mut raw, sequence)
        };
        record
            .pointer_mut(pointer)
            .expect("semantic fixture object")
            .as_object_mut()
            .expect("semantic object")
            .insert("future_field".into(), json!(true));
        assert_rejected(&jsonl(&raw)?, "unknown nested semantic field");
    }
    for kind in [
        json!({"kind":"user", "text":""}),
        json!({"kind":"assistant", "text":"", "tool_calls":[]}),
        json!({"kind":"system_prompt", "text":""}),
        json!({"kind":"tools", "tools":[]}),
        json!({"kind":"provider_state", "scope":{"provider":"p", "binding":"b"}, "state":null}),
        json!({"kind":"session_metadata", "name":null}),
    ] {
        let mut raw = one_entry(kind);
        raw[1]["data"]["future_field"] = json!(true);
        assert_rejected(&jsonl(&raw)?, "unknown entry data field");
    }
    let mut raw = records(BRANCHED)?;
    entry_mut(&mut raw, 9)["data"]["future_field"] = json!(true);
    assert_rejected(&jsonl(&raw)?, "unknown tool result field");
    selection_mut(&mut raw)["future_field"] = json!(true);
    // Remove the prior mutation to isolate selection unknown-field rejection.
    entry_mut(&mut raw, 9)["data"]
        .as_object_mut()
        .expect("data object")
        .remove("future_field");
    assert_rejected(&jsonl(&raw)?, "unknown selection field");
    let mut raw = base;
    raw[0]["forked_from"]["future_field"] = json!(true);
    assert_rejected(&jsonl(&raw)?, "unknown fork field");
    Ok(())
}

#[test]
fn required_nullable_fields_cannot_be_omitted() -> Result<(), Error> {
    for (sequence, pointer, fields) in [
        (
            0,
            "",
            &[
                "type",
                "format_version",
                "session_id",
                "created_at_ms",
                "scope",
            ][..],
        ),
        (
            1,
            "",
            &[
                "type",
                "id",
                "parent_id",
                "sequence",
                "timestamp_ms",
                "data",
            ][..],
        ),
        (1, "/data", &["kind", "text"][..]),
        (7, "/data", &["kind", "text", "tool_calls"][..]),
        (7, "/data/tool_calls/0", &["id", "name", "arguments"][..]),
        (7, "/data/metadata", &["format", "value"][..]),
        (
            7,
            "/data/attribution",
            &["provider", "binding", "model"][..],
        ),
        (
            2,
            "/data/tools/0",
            &["name", "description", "input_schema"][..],
        ),
        (9, "/data", &["assistant_id", "call_id", "output"][..]),
        (12, "/data", &["scope", "state"][..]),
        (12, "/data/scope", &["provider", "binding"][..]),
        (12, "/data/state", &["format", "value"][..]),
        (18, "/data", &["name"][..]),
    ] {
        for field in fields {
            let mut raw = records(BRANCHED)?;
            let record = if sequence == 0 {
                &mut raw[0]
            } else {
                entry_mut(&mut raw, sequence)
            };
            record
                .pointer_mut(pointer)
                .expect("fixture object")
                .as_object_mut()
                .expect("object")
                .remove(*field);
            assert_rejected(&jsonl(&raw)?, field);
        }
    }
    for field in ["type", "head", "revision"] {
        let mut raw = records(EMPTY)?;
        selection_mut(&mut raw)
            .as_object_mut()
            .expect("selection object")
            .remove(field);
        assert_rejected(&jsonl(&raw)?, "missing selection field");
    }
    for field in ["session_id", "entry_id"] {
        let mut raw = records(SELECTED)?;
        raw[0]["forked_from"]
            .as_object_mut()
            .expect("fork object")
            .remove(field);
        assert_rejected(&jsonl(&raw)?, "missing fork field");
    }
    // Omitting optional fields is permitted, unlike omitting required nullables.
    let raw = one_entry(json!({"kind":"assistant", "text":"", "tool_calls":[]}));
    ConversationHistory::from_jsonl(&jsonl(&raw)?)?.validate()?;
    for data in [
        json!({"kind":"tools", "tools":[]}),
        json!({"kind":"session_metadata", "name":""}),
    ] {
        ConversationHistory::from_jsonl(&jsonl(&one_entry(data))?)?.validate()?;
    }
    Ok(())
}

#[test]
fn semantic_structs_and_object_only_fields_reject_tuple_arrays_and_scalars() -> Result<(), Error> {
    for (sequence, pointer) in [
        (0, ""),
        (1, ""),
        (1, "/data"),
        (7, "/data/tool_calls/0"),
        (7, "/data/metadata"),
        (7, "/data/attribution"),
        (2, "/data/tools/0"),
        (12, "/data/scope"),
        (12, "/data/state"),
        (7, "/data/tool_calls/0/arguments"),
        (2, "/data/tools/0/input_schema"),
    ] {
        for value in [
            Value::Null,
            json!([]),
            json!("object"),
            json!(1),
            json!(true),
        ] {
            // Nullable replay/attribution and an explicit state reset are valid.
            if value.is_null()
                && matches!(
                    pointer,
                    "/data/metadata" | "/data/attribution" | "/data/state"
                )
            {
                continue;
            }
            let mut raw = records(BRANCHED)?;
            let record = if sequence == 0 {
                &mut raw[0]
            } else {
                entry_mut(&mut raw, sequence)
            };
            let label = format!(
                "semantic object at sequence {sequence}, {pointer}, null={}",
                value.is_null()
            );
            *record.pointer_mut(pointer).expect("fixture object") = value;
            assert_rejected(&jsonl(&raw)?, &label);
        }
    }
    for (sequence, pointer, tuple) in [
        (7, "/data/tool_calls/0", json!(["call-a", "echo", {}])),
        (7, "/data/metadata", json!(["synthetic.replay.v1", {}])),
        (
            7,
            "/data/attribution",
            json!(["fixture", "account-a/config-1", "synthetic-model"]),
        ),
        (2, "/data/tools/0", json!(["echo", "description", {}])),
        (12, "/data/scope", json!(["fixture", "account-a/config-1"])),
        (12, "/data/state", json!(["synthetic.state.v1", {}])),
    ] {
        let mut raw = records(BRANCHED)?;
        *entry_mut(&mut raw, sequence)
            .pointer_mut(pointer)
            .expect("fixture object") = tuple;
        assert_rejected(&jsonl(&raw)?, "serde struct tuple fallback");
    }
    let mut raw = records(SELECTED)?;
    raw[0]["forked_from"] = json!(["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", null]);
    assert_rejected(&jsonl(&raw)?, "fork tuple fallback");
    for tuple in [json!([null, 0]), json!([]), Value::Null] {
        let mut raw = records(EMPTY)?;
        *selection_mut(&mut raw) = tuple;
        assert_rejected(&jsonl(&raw)?, "selection is object-only");
    }
    // Provider state itself is a required nullable ProviderData object.
    let mut raw = records(BRANCHED)?;
    entry_mut(&mut raw, 12)["data"]["state"] = json!(["tuple", null]);
    assert_rejected(&jsonl(&raw)?, "provider state tuple");
    Ok(())
}

#[test]
fn invalid_field_types_do_not_coerce() -> Result<(), Error> {
    for (sequence, pointer, value) in [
        (0, "/name", json!(1)),
        (0, "/workspace", json!([])),
        (0, "/scope", Value::Null),
        (0, "/forked_from", json!(false)),
        (4, "/data/text", Value::Null),
        (7, "/data/text", json!({})),
        (7, "/data/tool_calls", Value::Null),
        (7, "/data/tool_calls", json!({})),
        (7, "/data/tool_calls/0/id", json!(1)),
        (7, "/data/tool_calls/0/name", Value::Null),
        (7, "/data/metadata/format", json!(1)),
        (7, "/data/attribution/model", json!(false)),
        (2, "/data/tools", json!({})),
        (2, "/data/tools/0/description", Value::Null),
        (9, "/data/call_id", json!(1)),
        (18, "/data/name", json!(false)),
    ] {
        let mut raw = records(BRANCHED)?;
        let record = if sequence == 0 {
            &mut raw[0]
        } else {
            entry_mut(&mut raw, sequence)
        };
        // Header optional fields are always present in this fixture.
        *record.pointer_mut(pointer).expect("fixture field") = value;
        assert_rejected(&jsonl(&raw)?, "wrong field type");
    }
    for (index, field) in [
        (0, "created_at_ms"),
        (0, "format_version"),
        (1, "sequence"),
        (1, "timestamp_ms"),
        (2, "revision"),
    ] {
        for literal in [
            "-1",
            "1.0",
            "1e0",
            "18446744073709551616",
            "null",
            "true",
            "\"1\"",
        ] {
            let mut raw = one_entry(json!({"kind":"user", "text":""}));
            raw[index][field] = json!("RAW_NUMBER_SENTINEL");
            let text = jsonl(&raw)?.replace("\"RAW_NUMBER_SENTINEL\"", literal);
            assert_rejected(&text, "integer field requires exact unsigned integer");
        }
    }
    Ok(())
}

#[test]
fn every_identity_position_requires_canonical_nonnil_uuid() -> Result<(), Error> {
    let canonical = "abcdefab-cdef-4abc-8def-abcdefabcdef";
    for (fixture, original) in [
        (BRANCHED, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        (BRANCHED, "00000000-0000-4000-8000-000000000001"),
        (BRANCHED, "00000000-0000-4000-8000-000000000007"),
        (BRANCHED, "00000000-0000-4000-8000-000000000020"),
        (SELECTED, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        (SELECTED, "ffffffff-ffff-4fff-8fff-ffffffffffff"),
    ] {
        // Rewrite every matching reference as well as the identity itself so
        // failure cannot be attributed only to an unresolved graph reference.
        let valid = fixture.replace(original, canonical);
        ConversationHistory::from_jsonl(&valid)?.validate()?;
        for invalid in [
            NIL_ID.to_owned(),
            canonical.to_uppercase(),
            canonical.replace('-', ""),
            format!("{{{canonical}}}"),
            format!("urn:uuid:{canonical}"),
            "not-a-uuid".into(),
            "1".into(),
        ] {
            assert_rejected(
                &valid.replace(canonical, &invalid),
                "noncanonical or nil identity",
            );
        }
    }
    for (sequence, pointer) in [(0, "/session_id"), (1, "/id"), (9, "/data/assistant_id")] {
        let mut raw = records(BRANCHED)?;
        let record = if sequence == 0 {
            &mut raw[0]
        } else {
            entry_mut(&mut raw, sequence)
        };
        *record.pointer_mut(pointer).expect("identity") = Value::Null;
        assert_rejected(&jsonl(&raw)?, "nonnullable identity");
    }
    Ok(())
}

#[test]
fn duplicate_keys_are_rejected_even_in_opaque_json() -> Result<(), Error> {
    for (fixture, original, duplicated) in [
        (
            BRANCHED,
            "\"type\":\"session\"",
            "\"type\":\"session\",\"type\":\"session\"",
        ),
        (
            BRANCHED,
            "\"format_version\":1",
            "\"format_version\":1,\"format_version\":2",
        ),
        (
            BRANCHED,
            "\"sequence\":20",
            "\"sequence\":20,\"sequence\":20",
        ),
        (
            BRANCHED,
            "\"kind\":\"user\"",
            "\"kind\":\"user\",\"kind\":\"user\"",
        ),
        (BRANCHED, "\"head\":", "\"head\":null,\"head\":"),
        (
            BRANCHED,
            "\"id\":\"call-a\"",
            "\"id\":\"call-a\",\"id\":\"call-a\"",
        ),
        (
            BRANCHED,
            "\"description\":\"Synthetic definition\"",
            "\"description\":\"Synthetic definition\",\"description\":\"Synthetic definition\"",
        ),
        (
            BRANCHED,
            "\"format\":\"synthetic.replay.v1\"",
            "\"format\":\"synthetic.replay.v1\",\"format\":\"other\"",
        ),
        (
            BRANCHED,
            "\"model\":\"synthetic-model\"",
            "\"model\":\"synthetic-model\",\"model\":\"synthetic-model\"",
        ),
        (
            BRANCHED,
            "\"binding\":\"account-b/config-2\"",
            "\"binding\":\"account-b/config-2\",\"binding\":\"account-b/config-2\"",
        ),
        (SELECTED, "\"entry_id\":", "\"entry_id\":null,\"entry_id\":"),
        // These duplicates would disappear if parsing first normalized to Value.
        (
            BRANCHED,
            "\"provider_extension\":true",
            "\"provider_extension\":true,\"provider_extension\":false",
        ),
        (
            BRANCHED,
            "\"anything\":true",
            "\"anything\":true,\"anything\":true",
        ),
        (
            BRANCHED,
            "\"integer\":18446744073709551615",
            "\"integer\":1,\"integer\":18446744073709551615",
        ),
        (
            BRANCHED,
            "\"thread\":\"selected-branch\"",
            "\"thread\":\"selected-branch\",\"thread\":\"changed\"",
        ),
        (
            BRANCHED,
            "\"branch\":\"selected\"",
            "\"branch\":\"selected\",\"branch\":\"selected\"",
        ),
    ] {
        assert!(
            fixture.contains(original),
            "duplicate mutation must affect a fixture"
        );
        let malformed = fixture.replacen(original, duplicated, 1);
        assert_rejected(&malformed, "duplicate JSON key");
    }
    // Escaped spellings of the same key are also duplicates.
    assert_rejected(
        &EMPTY.replacen("\"revision\":0", "\"revision\":0,\"revi\\u0073ion\":0", 1),
        "escaped duplicate key",
    );
    Ok(())
}

fn padded_entry(sequence: u64, bytes: usize) -> Result<String, Error> {
    let mut entry = one_entry(json!({"kind":"user", "text":""}))[1].clone();
    entry["id"] = json!(Uuid::from_u128(u128::from(sequence)));
    entry["sequence"] = json!(sequence);
    let overhead = serde_json::to_string(&entry)?.len();
    let available = bytes
        .checked_sub(overhead)
        .expect("target record exceeds fixed overhead");
    // Multibyte content detects character-count limits mistaken for byte limits.
    entry["data"]["text"] = json!(format!(
        "{}{}",
        "é".repeat(available / 2),
        "x".repeat(available % 2)
    ));
    let text = serde_json::to_string(&entry)?;
    assert_eq!(text.len(), bytes);
    Ok(text)
}

#[test]
fn record_limit_is_utf8_bytes_before_lf_and_applies_to_export() -> Result<(), Error> {
    assert_eq!(MAX_RECORD_BYTES, 1_048_576);
    let raw = one_entry(json!({"kind":"user", "text":""}));
    // padded_entry uses deterministic UUID 1, so selection must use that identity.
    let mut selection = raw[2].clone();
    selection["head"] = json!(Uuid::from_u128(1));
    let header = serde_json::to_string(&raw[0])?;
    let selection = serde_json::to_string(&selection)?;
    let exact = padded_entry(1, MAX_RECORD_BYTES)?;
    let valid = format!("{header}\n{exact}\n{selection}\n");
    let mut history = ConversationHistory::from_jsonl(&valid)?;
    history.to_jsonl()?;
    // CR is still a byte before LF, even though ordinary CRLF is accepted.
    let cr_overflow = format!("{header}\n{exact}\r\n{selection}\n");
    assert_eq!(
        assert_rejected(&cr_overflow, "record plus CR exceeds limit"),
        HistoryError::LimitExceeded
    );
    let oversized = format!(
        "{header}\n{}\n{selection}\n",
        padded_entry(1, MAX_RECORD_BYTES + 1)?
    );
    assert_eq!(
        assert_rejected(&oversized, "oversized record"),
        HistoryError::LimitExceeded
    );
    if let EntryData::User { text } = &mut history.entries[0].data {
        text.push('x');
    } else {
        panic!("expected user payload");
    }
    assert!(matches!(
        history.to_jsonl(),
        Err(HistoryError::LimitExceeded)
    ));
    Ok(())
}

#[test]
fn snapshot_limit_counts_all_records_and_delimiters() -> Result<(), Error> {
    assert_eq!(MAX_SNAPSHOT_BYTES, 67_108_864);
    let raw = records(EMPTY)?;
    let mut text = String::with_capacity(MAX_SNAPSHOT_BYTES + 1024);
    text.push_str(&serde_json::to_string(&raw[0])?);
    text.push('\n');
    // Every individual record is within its limit and every ID/sequence is valid;
    // the total alone exceeds the independent snapshot limit.
    for sequence in 1..=64 {
        text.push_str(&padded_entry(sequence, MAX_RECORD_BYTES)?);
        text.push('\n');
    }
    text.push_str("{\"type\":\"selection\",\"head\":null,\"revision\":64}\n");
    assert!(text.len() > MAX_SNAPSHOT_BYTES);
    assert_eq!(
        assert_rejected(&text, "oversized complete snapshot"),
        HistoryError::LimitExceeded
    );
    Ok(())
}

#[test]
fn entry_count_limit_cannot_be_bypassed_by_small_records_or_direct_construction()
-> Result<(), Error> {
    assert_eq!(MAX_ENTRIES, 100_000);
    let mut history = ConversationHistory::from_jsonl(EMPTY)?;
    history.selection.revision = MAX_ENTRIES as u64;
    for index in 1..=MAX_ENTRIES {
        history.entries.push(HistoryEntry {
            id: EntryId(Uuid::from_u128(index as u128)),
            parent_id: None,
            sequence: index as u64,
            timestamp_ms: 0,
            data: EntryData::User {
                text: String::new(),
            },
        });
    }
    history.validate()?;
    let valid = history.to_jsonl()?;
    assert!(valid.len() < MAX_SNAPSHOT_BYTES);
    assert_eq!(
        ConversationHistory::from_jsonl(&valid)?.entries.len(),
        MAX_ENTRIES
    );
    let extra = HistoryEntry {
        id: EntryId(Uuid::from_u128((MAX_ENTRIES + 1) as u128)),
        parent_id: None,
        sequence: (MAX_ENTRIES + 1) as u64,
        timestamp_ms: 0,
        data: EntryData::User {
            text: String::new(),
        },
    };
    history.selection.revision += 1;
    history.entries.push(extra.clone());
    assert!(matches!(
        history.validate(),
        Err(HistoryError::LimitExceeded)
    ));
    assert!(matches!(
        history.to_jsonl(),
        Err(HistoryError::LimitExceeded)
    ));
    let (prefix, _) = valid
        .rsplit_once("{\"type\":\"selection\"")
        .expect("canonical selection is final");
    let excessive = format!(
        "{prefix}{}\n{}\n",
        serde_json::to_string(&HistoryRecord::Entry(extra))?,
        serde_json::to_string(&HistoryRecord::Selection(history.selection.clone()))?
    );
    assert_eq!(
        assert_rejected(&excessive, "too many small entries"),
        HistoryError::LimitExceeded
    );
    Ok(())
}

#[test]
fn nesting_errors_and_error_messages_do_not_expose_content() -> Result<(), Error> {
    let mut raw = one_entry(
        json!({"kind":"assistant", "text":"SYNTHETIC_PROMPT_MARKER", "tool_calls":[],
        "metadata":{"format":"synthetic", "value":"SYNTHETIC_PROVIDER_MARKER"}, "future_field":true}),
    );
    let malformed = jsonl(&raw)?;
    let error = assert_rejected(
        &malformed,
        "unknown semantic field with sensitive-like markers",
    );
    for formatted in [error.to_string(), format!("{error:?}")] {
        assert!(!formatted.contains("SYNTHETIC_PROMPT_MARKER"));
        assert!(!formatted.contains("SYNTHETIC_PROVIDER_MARKER"));
        assert!(!formatted.contains("future_field"));
    }
    raw[1]["data"]
        .as_object_mut()
        .expect("data object")
        .remove("future_field");
    raw[1]["data"]["metadata"]["value"] = json!("RAW_NESTING_SENTINEL");
    let deep = format!("{}null{}", "[".repeat(512), "]".repeat(512));
    let nested = jsonl(&raw)?.replace("\"RAW_NESTING_SENTINEL\"", &deep);
    assert_rejected(&nested, "bounded JSON nesting");
    for error in [
        HistoryError::InvalidRecord,
        HistoryError::UnsupportedVersion,
        HistoryError::InvalidIdentity,
        HistoryError::InvalidReference,
        HistoryError::InvalidOrder,
        HistoryError::InvalidSelection,
        HistoryError::InvalidScope,
        HistoryError::InvalidData,
        HistoryError::LimitExceeded,
        HistoryError::IncompleteSnapshot,
    ] {
        let copied = error;
        assert_eq!(copied, error);
        let _: &dyn std::error::Error = &error;
        assert!(!error.to_string().is_empty());
        assert!(error.to_string().len() < 256);
    }
    Ok(())
}
