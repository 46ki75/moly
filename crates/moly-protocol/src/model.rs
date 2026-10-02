//! Experimental Model Provider protocol v1, carried by the common envelope v1.
//! See `docs/model-provider-protocol.md` and `conformance/schemas/model-provider-v1.json`.
use crate::{ModelCallId, RunId, SessionId, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Independently versioned Model Provider semantics.
pub const PROVIDER_VERSION: u16 = 1;

/// Resolved process launch data. The Server never searches PATH or invokes a shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentCommand {
    /// Absolute executable path, resolved by the Client.
    pub executable: String,
    /// Literal arguments, without shell expansion.
    #[serde(default)]
    pub args: Vec<String>,
    /// Complete explicitly supplied environment, not inherited Server configuration.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Client-selected implementation and implementation-specific configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// How the Server launches this implementation.
    pub command: ComponentCommand,
    /// Opaque provider options. The selected implementation validates their semantics.
    pub options: Value,
}

/// Handshake result. The Server checks both role and semantic version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInitialized {
    /// Must be `model_provider` for this protocol.
    pub role: String,
    /// Accepted Model Provider version.
    pub protocol_version: u16,
}

/// Purpose of one Server-authorized inference step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    /// Primary conversation inference.
    Primary,
}

/// Logical identities independent of provider HTTP or process identifiers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InferenceContext {
    /// Owning conversation.
    pub session_id: SessionId,
    /// Owning live run.
    pub run_id: RunId,
    /// Unique Server-authorized inference step.
    pub model_call_id: ModelCallId,
    /// Purpose of this step.
    pub call_kind: CallKind,
}

/// Provider-owned replay data, never interpreted by Core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMetadata {
    /// Identifies the representation, not the process that produced it.
    pub format: String,
    /// Opaque data; may contain sensitive model content.
    pub value: Value,
}

/// Model request for a hosted tool, not execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostToolCall {
    /// Nonempty provider correlation, unique within the assistant message.
    pub id: String,
    /// Advertised capability name.
    pub name: String,
    /// Parsed JSON object, not a provider-specific serialized string.
    pub arguments: Value,
}

/// Provider-neutral model context. History storage is a separate contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelMessage {
    /// Accepted user input.
    User {
        /// Text input.
        text: String,
    },
    /// Normalized model output and opaque replay information.
    Assistant {
        /// Optional text accompanying a tool request.
        text: Option<String>,
        /// Requested hosted capabilities.
        tool_calls: Vec<HostToolCall>,
        /// Provider-owned replay data, if needed.
        metadata: Option<ProviderMetadata>,
    },
    /// Completed hosted tool result.
    ToolResult {
        /// Correlation from the originating assistant message.
        call_id: String,
        /// Structured result, encoded for the upstream API by the Provider.
        output: Value,
    },
}

/// Exactly one authorized, nonstreaming model step. Never log this structure.
#[derive(Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Provider-specific resolved options, without configuration discovery.
    pub options: Value,
    /// Value resolved from the selected secret reference only; not persisted.
    pub credential: Option<String>,
    /// Server-assigned authority and tracing identities.
    pub context: InferenceContext,
    /// Selected model context, not the entire history tree.
    pub messages: Vec<ModelMessage>,
    /// Hosted capabilities currently authorized by the Server.
    pub tools: Vec<ToolDefinition>,
}

/// Semantic outcome, not an event or authorization to execute tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ModelStep {
    /// Final assistant text.
    Completed {
        /// Complete text, including an explicitly empty response.
        text: String,
        /// Opaque replay information.
        metadata: Option<ProviderMetadata>,
    },
    /// Server must assign ToolRun authority before executing these calls.
    AwaitHostTools {
        /// Optional accompanying assistant text.
        text: Option<String>,
        /// Nonempty, validated advertised hosted-tool requests.
        calls: Vec<HostToolCall>,
        /// Opaque replay information.
        metadata: Option<ProviderMetadata>,
    },
}
