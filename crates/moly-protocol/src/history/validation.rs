use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Index<'a> {
    entries: BTreeMap<EntryId, &'a HistoryEntry>,
}

fn nonblank(value: &str) -> bool {
    !value.trim().is_empty()
}

fn provider_data(value: &Option<ProviderData>) -> bool {
    value.as_ref().is_none_or(|data| nonblank(&data.format))
}

fn value_depth(value: &Value) -> Result<(), HistoryError> {
    let mut stack = vec![(value, 0usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > MAX_VALUE_DEPTH {
            return Err(HistoryError::LimitExceeded);
        }
        match value {
            Value::Array(values) => stack.extend(values.iter().map(|value| (value, depth + 1))),
            Value::Object(values) => stack.extend(values.values().map(|value| (value, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

fn data(entry: &HistoryEntry) -> Result<(), HistoryError> {
    // Check caller-constructed values before serialization: Serde's encoder has
    // no recursion limit, and successful exports must remain decodable.
    match &entry.data {
        EntryData::Assistant {
            tool_calls,
            metadata,
            ..
        } => {
            for call in tool_calls {
                value_depth(&call.arguments)?;
            }
            if let Some(metadata) = metadata {
                value_depth(&metadata.value)?;
            }
        }
        EntryData::ToolResult { output, .. } => value_depth(output)?,
        EntryData::Tools { tools } => {
            for tool in tools {
                value_depth(&tool.input_schema)?;
            }
        }
        EntryData::ProviderState {
            state: Some(state), ..
        } => value_depth(&state.value)?,
        _ => {}
    }
    let valid = match &entry.data {
        EntryData::User { .. }
        | EntryData::SystemPrompt { .. }
        | EntryData::SessionMetadata { .. } => true,
        EntryData::Assistant {
            text,
            tool_calls,
            metadata,
            attribution,
        } => {
            let mut ids = BTreeSet::new();
            (text.is_some() || !tool_calls.is_empty())
                && tool_calls.iter().all(|call| {
                    nonblank(&call.id)
                        && nonblank(&call.name)
                        && call.arguments.is_object()
                        && ids.insert(&call.id)
                })
                && provider_data(metadata)
                && attribution.as_ref().is_none_or(|value| {
                    nonblank(&value.provider) && nonblank(&value.binding) && nonblank(&value.model)
                })
        }
        EntryData::ToolResult {
            assistant_id,
            call_id,
            ..
        } => !assistant_id.0.is_nil() && nonblank(call_id),
        EntryData::Tools { tools } => {
            let mut names = BTreeSet::new();
            tools.iter().all(|tool| {
                nonblank(&tool.name) && tool.input_schema.is_object() && names.insert(&tool.name)
            })
        }
        EntryData::ProviderState { scope, state } => {
            nonblank(&scope.provider) && nonblank(&scope.binding) && provider_data(state)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(HistoryError::InvalidData)
    }
}

pub(super) fn validate(history: &ConversationHistory) -> Result<Index<'_>, HistoryError> {
    if history.header.format_version != FORMAT_VERSION {
        return Err(HistoryError::UnsupportedVersion);
    }
    if history.entries.len() > MAX_ENTRIES {
        return Err(HistoryError::LimitExceeded);
    }
    if history.header.session_id.0.is_nil()
        || history.header.forked_from.as_ref().is_some_and(|origin| {
            origin.session_id.0.is_nil()
                || origin.session_id == history.header.session_id
                || origin.entry_id.is_some_and(|id| id.0.is_nil())
        })
    {
        return Err(HistoryError::InvalidIdentity);
    }
    let mut entries = BTreeMap::new();
    let mut sequences = BTreeSet::new();
    let mut children: BTreeMap<Option<EntryId>, Vec<EntryId>> = BTreeMap::new();
    for entry in &history.entries {
        if entry.id.0.is_nil()
            || entry.parent_id.is_some_and(|id| id.0.is_nil())
            || entries.insert(entry.id, entry).is_some()
        {
            return Err(HistoryError::InvalidIdentity);
        }
        if entry.sequence == 0
            || entry.sequence > history.selection.revision
            || !sequences.insert(entry.sequence)
        {
            return Err(HistoryError::InvalidOrder);
        }
        children.entry(entry.parent_id).or_default().push(entry.id);
        data(entry)?;
    }
    for entry in &history.entries {
        if let Some(parent) = entry.parent_id {
            let parent = entries.get(&parent).ok_or(HistoryError::InvalidReference)?;
            if parent.sequence >= entry.sequence {
                return Err(HistoryError::InvalidOrder);
            }
        }
    }
    if history
        .selection
        .head
        .is_some_and(|id| !entries.contains_key(&id))
    {
        return Err(HistoryError::InvalidSelection);
    }
    let index = Index { entries };
    let path = path(history, &index)?;
    if history.header.scope == ExportScope::SelectedBranch && path.len() != history.entries.len() {
        return Err(HistoryError::InvalidScope);
    }

    // Parent commit order proves acyclicity. Iterative DFS avoids call-stack
    // exhaustion and makes ancestry checks bounded rather than quadratic in depth.
    let mut intervals = BTreeMap::new();
    let mut stack: Vec<_> = children
        .get(&None)
        .into_iter()
        .flatten()
        .map(|id| (*id, false))
        .collect();
    let mut clock = 0usize;
    while let Some((id, exiting)) = stack.pop() {
        clock += 1;
        if exiting {
            let interval: &mut (usize, usize) = intervals
                .get_mut(&id)
                .ok_or(HistoryError::InvalidReference)?;
            interval.1 = clock;
        } else {
            intervals.insert(id, (clock, 0));
            stack.push((id, true));
            if let Some(children) = children.get(&Some(id)) {
                stack.extend(children.iter().map(|id| (*id, false)));
            }
        }
    }
    let calls: BTreeMap<_, BTreeSet<_>> = history
        .entries
        .iter()
        .filter_map(|entry| {
            if let EntryData::Assistant { tool_calls, .. } = &entry.data {
                Some((
                    entry.id,
                    tool_calls.iter().map(|call| call.id.as_str()).collect(),
                ))
            } else {
                None
            }
        })
        .collect();
    let mut results: BTreeMap<(EntryId, &str), Vec<(usize, usize)>> = BTreeMap::new();
    for entry in &history.entries {
        if let EntryData::ToolResult {
            assistant_id,
            call_id,
            ..
        } = &entry.data
        {
            let tool_calls = calls
                .get(assistant_id)
                .ok_or(HistoryError::InvalidReference)?;
            let (start, end) = *intervals
                .get(assistant_id)
                .ok_or(HistoryError::InvalidReference)?;
            let current = *intervals
                .get(&entry.id)
                .ok_or(HistoryError::InvalidReference)?;
            if !tool_calls.contains(call_id.as_str()) || start >= current.0 || end <= current.1 {
                return Err(HistoryError::InvalidReference);
            }
            results
                .entry((*assistant_id, call_id))
                .or_default()
                .push(current);
        }
    }
    for intervals in results.values_mut() {
        intervals.sort_unstable();
        if intervals.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(HistoryError::InvalidReference);
        }
    }
    codec::check_sizes(history)?;
    Ok(index)
}

fn path<'a>(
    history: &'a ConversationHistory,
    index: &Index<'a>,
) -> Result<Vec<&'a HistoryEntry>, HistoryError> {
    let mut path = Vec::new();
    let mut head = history.selection.head;
    while let Some(id) = head {
        let entry = *index
            .entries
            .get(&id)
            .ok_or(HistoryError::InvalidReference)?;
        path.push(entry);
        head = entry.parent_id;
    }
    path.reverse();
    Ok(path)
}

pub(super) fn selected_path(
    history: &ConversationHistory,
) -> Result<Vec<&HistoryEntry>, HistoryError> {
    path(history, &validate(history)?)
}
