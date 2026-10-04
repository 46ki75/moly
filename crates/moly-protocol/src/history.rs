//! Independently versioned conversation-history data, not MPP or runtime checkpoints.
//!
//! JSONL snapshots contain a session header, entries, and an explicit selection.
//! [`ConversationHistory::from_jsonl`](crate::history::ConversationHistory::from_jsonl)
//! validates structure and ancestry without I/O
//! or execution. Loading these values does not activate an Agent Server session.
//! See `docs/conversation-history.md` for scope, ownership, and security policy.
use crate::SessionId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use uuid::Uuid;

mod codec;
#[cfg(test)]
mod tests;
mod validation;

/// History format version, independent of all Component protocol versions.
pub const FORMAT_VERSION: u16 = 1;
/// Maximum UTF-8 bytes per history record, excluding LF.
pub const MAX_RECORD_BYTES: usize = 1_048_576;
/// Maximum snapshot bytes, including record delimiters.
pub const MAX_SNAPSHOT_BYTES: usize = 67_108_864;
/// Maximum entries in one portable snapshot.
pub const MAX_ENTRIES: usize = 100_000;
/// Maximum nested edges inside a JSON-valued payload, excluding record wrappers.
pub const MAX_VALUE_DEPTH: usize = 64;

/// Stable entry identity within a conversation, unrelated to execution identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EntryId(pub Uuid);
impl EntryId {
    /// Allocate an identity independent of storage locations and processes.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for EntryId {
    fn default() -> Self {
        Self::new()
    }
}
impl<'de> Deserialize<'de> for EntryId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        codec::canonical_uuid(deserializer).map(Self)
    }
}

/// Declared coverage of a portable snapshot, not proof of remote completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum ExportScope {
    /// All branches supplied by the producer.
    FullTree,
    /// Exactly the selected head's ancestry.
    SelectedBranch,
}
impl TryFrom<String> for ExportScope {
    type Error = HistoryError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "full_tree" => Ok(Self::FullTree),
            "selected_branch" => Ok(Self::SelectedBranch),
            _ => Err(HistoryError::InvalidScope),
        }
    }
}

/// External provenance; the source need not be included in this snapshot.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkOrigin {
    /// Source conversation, distinct from the fork's own identity.
    #[serde(deserialize_with = "codec::session_id")]
    pub session_id: SessionId,
    /// Source branch point, or its virtual root. Must be explicitly supplied.
    #[serde(deserialize_with = "codec::required_option")]
    pub entry_id: Option<EntryId>,
}

/// Conversation identity and initial non-model-visible metadata.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHeader {
    /// Independently negotiated history representation; currently one.
    pub format_version: u16,
    /// Preserved on restoration; replaced only by an explicit new session/fork.
    #[serde(deserialize_with = "codec::session_id")]
    pub session_id: SessionId,
    /// Creation time in Unix milliseconds, not canonical ordering.
    pub created_at_ms: u64,
    /// Complete tree or selected ancestry.
    pub scope: ExportScope,
    /// Optional human-facing name, never implicitly model context.
    pub name: Option<String>,
    /// Workspace hint, not permission to access a filesystem path.
    pub workspace: Option<String>,
    /// Optional source of an explicit fork.
    #[serde(default, deserialize_with = "codec::optional_object")]
    pub forked_from: Option<ForkOrigin>,
}

/// Nonsecret stable Provider/account/configuration namespace, not credentials.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderScope {
    /// Adapter/state namespace, not a concrete executable path.
    pub provider: String,
    /// Opaque nonsecret account/endpoint/configuration binding identifier.
    pub binding: String,
}

/// Opaque replay or continuation representation; producers must exclude secrets.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderData {
    /// Provider-owned versioned representation identifier.
    pub format: String,
    /// Sensitive model/replay data, not instructions for the history loader.
    pub value: Value,
}

/// Historical model attribution; recording it does not select a live Provider.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attribution {
    /// Provider namespace.
    pub provider: String,
    /// Nonsecret account/configuration binding.
    pub binding: String,
    /// Model used for the recorded output.
    pub model: String,
}

/// A recorded request for a tool, never live execution authority.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    /// Provider correlation, unique within one assistant entry.
    pub id: String,
    /// Historical capability name.
    pub name: String,
    /// Object-valued arguments, not upstream stringified JSON.
    pub arguments: Value,
}

/// Historical tool definition, independently versioned from the MPP DTO.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    /// Unique capability name within a tools entry.
    pub name: String,
    /// Historical model-facing description.
    pub description: String,
    /// Object-valued input schema; this crate does not evaluate JSON Schema.
    pub input_schema: Value,
}

/// Supported historical content; unknown semantic variants must not be discarded.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryData {
    /// Accepted user text.
    User {
        /// Original text, including an explicitly empty message.
        text: String,
    },
    /// Assistant text/tool intent and opaque replay data.
    Assistant {
        /// Nullable text; null requires at least one tool call.
        #[serde(deserialize_with = "codec::required_option")]
        text: Option<String>,
        /// Historical requests, not execution leases.
        #[serde(deserialize_with = "codec::objects")]
        tool_calls: Vec<ToolCall>,
        /// Optional Provider-native replay representation.
        #[serde(default, deserialize_with = "codec::optional_object")]
        metadata: Option<ProviderData>,
        /// Optional model/account attribution.
        #[serde(default, deserialize_with = "codec::optional_object")]
        attribution: Option<Attribution>,
    },
    /// Recorded outcome, including structured execution errors.
    ToolResult {
        /// Ancestor assistant that requested the tool; disambiguates reused call IDs.
        assistant_id: EntryId,
        /// Correlation within that assistant's calls.
        call_id: String,
        /// Structured output, not an instruction to repeat an effect.
        output: Value,
    },
    /// Historical system-prompt change.
    SystemPrompt {
        /// Prompt text.
        text: String,
    },
    /// Historical advertised-tool change.
    Tools {
        /// Definitions at this point in the selected branch.
        #[serde(deserialize_with = "codec::objects")]
        tools: Vec<ToolDefinition>,
    },
    /// Opaque noncredential continuation state at an explicit history position.
    ProviderState {
        /// Nonsecret Provider/account/configuration scope.
        #[serde(deserialize_with = "codec::object")]
        scope: ProviderScope,
        /// State or explicit reset. Presence is required; null clears earlier state.
        #[serde(deserialize_with = "codec::optional_object")]
        state: Option<ProviderData>,
    },
    /// Human-facing metadata, not model input.
    SessionMetadata {
        /// Updated name, or explicit clearing of the previous name.
        #[serde(deserialize_with = "codec::required_option")]
        name: Option<String>,
    },
}

/// Immutable typed entry; ordering and ancestry are independent of record position.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    /// Persistent entry identity.
    pub id: EntryId,
    /// Parent entry, or explicit virtual session root.
    #[serde(deserialize_with = "codec::required_option")]
    pub parent_id: Option<EntryId>,
    /// Positive unique Agent Server-assigned commit position; gaps are allowed.
    pub sequence: u64,
    /// Unix milliseconds, not a sorting key.
    pub timestamp_ms: u64,
    /// Historical content with no live execution authority.
    pub data: EntryData,
}

/// Explicit selected execution head at a consistent logical revision.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    /// Selected entry, or explicit virtual session root.
    #[serde(deserialize_with = "codec::required_option")]
    pub head: Option<EntryId>,
    /// Logical revision covering all entries; not a durability acknowledgment.
    pub revision: u64,
}

/// One JSONL record. Deserialization alone does not validate a complete snapshot.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryRecord {
    /// First and only session header.
    Session(SessionHeader),
    /// Historical entry, in any physical order.
    Entry(HistoryEntry),
    /// Last and only selection record.
    Selection(Selection),
}

/// Consistent portable snapshot, not an append journal or runtime checkpoint.
#[derive(Clone)]
pub struct ConversationHistory {
    /// Version, stable identity, scope, and initial metadata.
    pub header: SessionHeader,
    /// All entries declared by the snapshot scope.
    pub entries: Vec<HistoryEntry>,
    /// Explicit selected head and logical revision.
    pub selection: Selection,
}
impl ConversationHistory {
    /// Strictly decode and validate a bounded snapshot without storage or execution.
    pub fn from_jsonl(input: &str) -> Result<Self, HistoryError> {
        codec::decode(input)
    }

    /// Validate supported data, identities, ancestry, tool references, and selection.
    /// Does not prove upstream validity, live ownership, or restore compatibility.
    pub fn validate(&self) -> Result<(), HistoryError> {
        validation::validate(self).map(|_| ())
    }

    /// Return selected ancestry in root-to-head order, including non-message entries.
    /// This is not model-context reconstruction or permission to execute anything.
    pub fn selected_path(&self) -> Result<Vec<&HistoryEntry>, HistoryError> {
        validation::selected_path(self)
    }

    /// Validate and encode canonical JSONL, sorted by entry sequence, ending in LF.
    pub fn to_jsonl(&self) -> Result<String, HistoryError> {
        codec::encode(self)
    }
}

/// Static, redacted history failures. No prompts or Provider data enter diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryError {
    /// Invalid JSON/record shape, unsupported fields or data kinds, or duplicate keys.
    InvalidRecord,
    /// Unsupported independent history format version.
    UnsupportedVersion,
    /// Invalid or conflicting logical identities.
    InvalidIdentity,
    /// Missing/invalid ancestry or tool correlation.
    InvalidReference,
    /// Duplicate/invalid commit order or revision.
    InvalidOrder,
    /// Selected head does not resolve.
    InvalidSelection,
    /// Declared branch-only export includes entries outside the selected path.
    InvalidScope,
    /// Unsupported content invariants or empty identifiers.
    InvalidData,
    /// Record, snapshot, or entry-count bound exceeded.
    LimitExceeded,
    /// Missing header/selection/final delimiter, or records in an invalid phase.
    IncompleteSnapshot,
}
impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRecord => "Invalid history record",
            Self::UnsupportedVersion => "Unsupported history format version",
            Self::InvalidIdentity => "Invalid history identity",
            Self::InvalidReference => "Invalid history reference",
            Self::InvalidOrder => "Invalid history commit order",
            Self::InvalidSelection => "Invalid history selection",
            Self::InvalidScope => "Invalid history export scope",
            Self::InvalidData => "Invalid history content",
            Self::LimitExceeded => "History limit exceeded",
            Self::IncompleteSnapshot => "Incomplete history snapshot",
        })
    }
}
impl std::error::Error for HistoryError {}
