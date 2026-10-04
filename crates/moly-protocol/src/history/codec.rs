use super::*;
use serde::de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor};
use std::io;

pub(super) fn canonical_uuid<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Uuid, D::Error> {
    let text = String::deserialize(deserializer)?;
    let id = Uuid::parse_str(&text).map_err(|_| de::Error::custom("Invalid history identity"))?;
    if id.is_nil() || id.to_string() != text {
        return Err(de::Error::custom("Invalid history identity"));
    }
    Ok(id)
}

pub(super) fn session_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SessionId, D::Error> {
    canonical_uuid(deserializer).map(SessionId)
}

// A custom deserializer prevents Serde's missing-Option default from turning an
// omitted root/reset marker into an explicit null with different semantics.
pub(super) fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn checked_object<T: DeserializeOwned, E: de::Error>(value: Value) -> Result<T, E> {
    if !value.is_object() {
        return Err(E::custom("Expected history object"));
    }
    serde_json::from_value(value).map_err(|_| E::custom("Invalid history object"))
}

pub(super) fn object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    checked_object(Value::deserialize(deserializer)?)
}

pub(super) fn optional_object<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        Ok(None)
    } else {
        checked_object(value).map(Some)
    }
}

pub(super) fn objects<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    Vec::<Value>::deserialize(deserializer)?
        .into_iter()
        .map(checked_object)
        .collect()
}

pub(super) fn decode(input: &str) -> Result<ConversationHistory, HistoryError> {
    if input.len() > MAX_SNAPSHOT_BYTES {
        return Err(HistoryError::LimitExceeded);
    }
    if !input.ends_with('\n') {
        return Err(HistoryError::IncompleteSnapshot);
    }
    let mut header = None;
    let mut entries = Vec::new();
    let mut selection = None;
    for line in input.split_terminator('\n') {
        if line.len() > MAX_RECORD_BYTES {
            return Err(HistoryError::LimitExceeded);
        }
        check_integer_tokens(line)?;
        let value = serde_json::from_str::<UniqueValue>(line)
            .map_err(|_| HistoryError::InvalidRecord)?
            .0;
        if value.get("type").and_then(Value::as_str) == Some("session")
            && let Some(version) = value.get("format_version").and_then(Value::as_u64)
            && version != u64::from(FORMAT_VERSION)
        {
            return Err(HistoryError::UnsupportedVersion);
        }
        let record: HistoryRecord =
            serde_json::from_value(value).map_err(|_| HistoryError::InvalidRecord)?;
        match record {
            HistoryRecord::Session(value) if header.is_none() => header = Some(value),
            HistoryRecord::Entry(value) if header.is_some() && selection.is_none() => {
                if entries.len() == MAX_ENTRIES {
                    return Err(HistoryError::LimitExceeded);
                }
                entries.push(value);
            }
            HistoryRecord::Selection(value) if header.is_some() && selection.is_none() => {
                selection = Some(value);
            }
            _ => return Err(HistoryError::IncompleteSnapshot),
        }
    }
    let history = ConversationHistory {
        header: header.ok_or(HistoryError::IncompleteSnapshot)?,
        entries,
        selection: selection.ok_or(HistoryError::IncompleteSnapshot)?,
    };
    history.validate()?;
    Ok(history)
}

// Serde otherwise promotes overflowing integer tokens to rounded floats, even
// inside opaque data. Inspect only numeric lexemes; the JSON decoder still owns
// grammar validation. String contents (including escaped quotes) are untouched.
fn check_integer_tokens(line: &str) -> Result<(), HistoryError> {
    let bytes = line.as_bytes();
    let (mut position, mut in_string, mut escaped) = (0, false, false);
    while position < bytes.len() {
        let byte = bytes[position];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b'-' || byte.is_ascii_digit() {
            let start = position;
            position += 1;
            while position < bytes.len()
                && (bytes[position].is_ascii_digit()
                    || matches!(bytes[position], b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                position += 1;
            }
            let token = &line[start..position];
            if !token.contains(['.', 'e', 'E']) {
                let valid = if token.starts_with('-') {
                    token.parse::<i64>().is_ok()
                } else {
                    token.parse::<u64>().is_ok()
                };
                if !valid {
                    return Err(HistoryError::InvalidData);
                }
            }
            continue;
        }
        position += 1;
    }
    Ok(())
}

// Borrow records during encoding so a large opaque Provider value is never cloned.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RecordRef<'a> {
    Session(&'a SessionHeader),
    Entry(&'a HistoryEntry),
    Selection(&'a Selection),
}

struct RecordSize(usize);
impl io::Write for RecordSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_RECORD_BYTES {
            return Err(io::Error::other("History record limit exceeded"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn check_sizes(history: &ConversationHistory) -> Result<(), HistoryError> {
    let mut total = 0usize;
    for record in std::iter::once(RecordRef::Session(&history.header))
        .chain(history.entries.iter().map(RecordRef::Entry))
        .chain(std::iter::once(RecordRef::Selection(&history.selection)))
    {
        let mut size = RecordSize(0);
        serde_json::to_writer(&mut size, &record).map_err(|_| HistoryError::LimitExceeded)?;
        total = total.saturating_add(size.0 + 1);
        if total > MAX_SNAPSHOT_BYTES {
            return Err(HistoryError::LimitExceeded);
        }
    }
    Ok(())
}

pub(super) fn encode(history: &ConversationHistory) -> Result<String, HistoryError> {
    history.validate()?;
    let mut entries: Vec<_> = history.entries.iter().collect();
    entries.sort_unstable_by_key(|entry| entry.sequence);
    let mut output = String::new();
    for record in std::iter::once(RecordRef::Session(&history.header))
        .chain(entries.into_iter().map(RecordRef::Entry))
        .chain(std::iter::once(RecordRef::Selection(&history.selection)))
    {
        let line = serde_json::to_string(&record).map_err(|_| HistoryError::InvalidRecord)?;
        output.push_str(&line);
        output.push('\n');
    }
    Ok(output)
}

// Value's ordinary deserializer silently keeps one of duplicate object fields.
// Reject ambiguity before typed decoding, even inside opaque replay data.
struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("unambiguous JSON")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|value| UniqueValue(Value::Number(value)))
                    .ok_or_else(|| E::custom("Invalid history number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value.into())))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<UniqueValue>()? {
                    values.push(value.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("Duplicate history field"));
                    }
                    values.insert(key, map.next_value::<UniqueValue>()?.0);
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}
