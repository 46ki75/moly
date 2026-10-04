use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn history() -> Result<ConversationHistory, HistoryError> {
    ConversationHistory::from_jsonl(concat!(
        "{\"type\":\"session\",\"format_version\":1,\"session_id\":\"00000000-0000-4000-8000-000000000001\",\"created_at_ms\":0,\"scope\":\"full_tree\"}\n",
        "{\"type\":\"entry\",\"id\":\"00000000-0000-4000-8000-000000000002\",\"parent_id\":null,\"sequence\":1,\"timestamp_ms\":0,\"data\":{\"kind\":\"user\",\"text\":\"synthetic\"}}\n",
        "{\"type\":\"selection\",\"head\":\"00000000-0000-4000-8000-000000000002\",\"revision\":1}\n"
    ))
}

#[test]
fn encoder_rejects_opaque_nesting_that_cannot_be_restored() -> TestResult {
    let mut history = history()?;
    let mut value = Value::Null;
    for _ in 0..200 {
        value = Value::Array(vec![value]);
    }
    history.entries[0].data = EntryData::ProviderState {
        scope: ProviderScope {
            provider: "fixture.v1".into(),
            binding: "account-fixture".into(),
        },
        state: Some(ProviderData {
            format: "fixture.session.v1".into(),
            value,
        }),
    };
    assert!(matches!(
        history.to_jsonl(),
        Err(HistoryError::LimitExceeded)
    ));
    Ok(())
}

#[test]
fn opaque_finite_numbers_survive_snapshot_roundtrip() -> TestResult {
    let mut bits = 0x1234_5678_9abc_def0u64;
    let mut numbers = Vec::new();
    for _ in 0..1000 {
        bits = bits.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        if let Some(number) = serde_json::Number::from_f64(f64::from_bits(bits)) {
            numbers.push(Value::Number(number));
        }
    }
    let value = Value::Array(numbers);
    let mut history = history()?;
    history.entries[0].data = EntryData::ProviderState {
        scope: ProviderScope {
            provider: "fixture.v1".into(),
            binding: "account-fixture".into(),
        },
        state: Some(ProviderData {
            format: "fixture.numbers.v1".into(),
            value: value.clone(),
        }),
    };
    let restored = ConversationHistory::from_jsonl(&history.to_jsonl()?)?;
    let EntryData::ProviderState {
        state: Some(state), ..
    } = &restored.entries[0].data
    else {
        return Err("missing synthetic Provider state".into());
    };
    assert!(
        state.value == value,
        "opaque finite numbers changed on roundtrip"
    );
    Ok(())
}

#[test]
fn out_of_range_integer_tokens_are_not_rounded_into_floats() -> TestResult {
    let snapshot = history()?.to_jsonl()?;
    for token in ["18446744073709551616", "-9223372036854775809"] {
        let data = format!(
            "\"kind\":\"provider_state\",\"scope\":{{\"provider\":\"fixture\",\"binding\":\"account\"}},\"state\":{{\"format\":\"fixture.v1\",\"value\":{token}}}"
        );
        let source = snapshot.replace("\"kind\":\"user\",\"text\":\"synthetic\"", &data);
        assert!(ConversationHistory::from_jsonl(&source).is_err());
    }
    Ok(())
}

#[test]
fn numeric_lexemes_in_strings_and_exact_integer_bounds_are_preserved() -> TestResult {
    let mut history = history()?;
    let value = serde_json::json!({
        "text": "é \\\" -9223372036854775809 18446744073709551616 \\\",",
        "negative": i64::MIN, "positive": u64::MAX,
        "subnormal": f64::from_bits(1), "large_float": 1e300,
    });
    history.entries[0].data = EntryData::ProviderState {
        scope: ProviderScope {
            provider: "fixture.v1".into(),
            binding: "account-fixture".into(),
        },
        state: Some(ProviderData {
            format: "fixture.numbers.v1".into(),
            value: value.clone(),
        }),
    };
    history.selection.revision = u64::MAX;
    let decoded = ConversationHistory::from_jsonl(&history.to_jsonl()?)?;
    assert_eq!(decoded.selection.revision, u64::MAX);
    let EntryData::ProviderState {
        state: Some(state), ..
    } = &decoded.entries[0].data
    else {
        return Err("missing synthetic Provider state".into());
    };
    assert!(state.value == value);
    Ok(())
}

#[test]
fn missing_nullable_markers_are_not_silently_defaulted() -> TestResult {
    let snapshot = history()?.to_jsonl()?;
    for missing in [
        snapshot.replace("\"parent_id\":null,", ""),
        snapshot.replace("\"head\":\"00000000-0000-4000-8000-000000000002\",", ""),
        snapshot.replace("\"kind\":\"user\",\"text\":\"synthetic\"", "\"kind\":\"provider_state\",\"scope\":{\"provider\":\"fixture\",\"binding\":\"account\"}"),
        snapshot.replace("\"kind\":\"user\",\"text\":\"synthetic\"", "\"kind\":\"session_metadata\""),
    ] {
        assert!(ConversationHistory::from_jsonl(&missing).is_err());
    }
    Ok(())
}

#[test]
fn long_ancestry_is_iterative_and_entry_count_is_bounded() -> TestResult {
    let mut history = history()?;
    history.entries.clear();
    let mut parent_id = None;
    for sequence in 1..=10_000 {
        let id = EntryId(Uuid::from_u128(u128::from(sequence)));
        history.entries.push(HistoryEntry {
            id,
            parent_id,
            sequence,
            timestamp_ms: 0,
            data: EntryData::User {
                text: String::new(),
            },
        });
        parent_id = Some(id);
    }
    history.selection.head = parent_id;
    history.selection.revision = 10_000;
    assert_eq!(history.selected_path()?.len(), 10_000);
    let entry = history.entries[0].clone();
    history.entries.resize(MAX_ENTRIES + 1, entry);
    assert_eq!(history.validate().err(), Some(HistoryError::LimitExceeded));
    Ok(())
}
