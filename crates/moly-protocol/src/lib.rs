//! Experimental Component schemas. No transport, process, or configuration discovery.
/// Model Provider protocol and provider-neutral model context.
pub mod model;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Common envelope version, independent of Component semantic versions.
pub const VERSION: u16 = 1;
/// Client–Server semantic version. V2 requires explicit Provider launch configuration.
pub const SERVER_VERSION: u16 = 2;
/// Maximum JSON payload bytes, excluding its terminating LF.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

macro_rules! identity {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            /// Allocate a logical identity independent of OS resources.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
identity!(
    ServerId,
    "Live Server incarnation identity; not a PID or endpoint."
);
identity!(ConnectionId, "One attached peer connection; not a session.");
identity!(
    SessionId,
    "Conversation identity independent of connections."
);
identity!(RunId, "Server-owned run identity.");
identity!(ModelCallId, "One Server-authorized inference step.");
identity!(ToolRunId, "Server-owned tool invocation; not a child PID.");
identity!(ExecutorId, "Logical side-effect executor; not a process.");

/// A transport-independent protocol envelope. Unknown fields are tolerated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Envelope version, not the negotiated Component semantic version.
    pub version: u16,
    /// Request, response, error, or event.
    #[serde(flatten)]
    pub body: Body,
}
impl Message {
    /// Wrap a body using the current protocol version.
    pub fn new(body: Body) -> Self {
        Self {
            version: VERSION,
            body,
        }
    }
}
/// Full-duplex messages. IDs are unique among outstanding requests in each direction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    /// Request that must receive one response or correlated error.
    Request {
        /// Direction-local correlation ID.
        id: u64,
        /// Semantic operation name.
        method: String,
        /// Operation-specific arguments.
        #[serde(default)]
        params: Value,
    },
    /// Successful correlated response.
    Response {
        /// Original request ID.
        id: u64,
        /// Operation-specific value.
        result: Value,
    },
    /// Correlated operation error, or fatal connection error with no ID.
    Error {
        /// Original request ID, absent for framing/connection errors.
        id: Option<u64>,
        /// Structured failure without secrets or implementation diagnostics.
        error: ProtocolError,
    },
    /// Unsolicited notification.
    Event {
        /// Event stream name, initially `session.event`.
        event: String,
        /// Typed event payload.
        params: Value,
    },
}
/// Structured protocol/semantic failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    /// Stable machine-readable category; see `docs/protocol.md`.
    pub code: String,
    /// Human-readable description. Never contains credentials or prompts.
    pub message: String,
}
impl ProtocolError {
    /// Construct an error without exposing internal error sources.
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ProtocolError {}

/// Server identity returned by initialize, never inferred from the endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Initialized {
    /// The authority accepting this connection.
    pub server_id: ServerId,
    /// Explicit role compatibility check.
    pub role: String,
    /// Version accepted for this connection.
    pub protocol_version: u16,
}
/// Fully resolved data; the Server performs no file discovery or env expansion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedConfig {
    /// Selected Model Provider, launch data, and opaque implementation options.
    pub provider: model::ProviderConfig,
    /// Absolute workspace root used by the bundled file-reading executor.
    pub workspace: String,
    /// Optional key in the generic host SecretStore, not a credential value.
    pub secret_ref: Option<String>,
}
/// Compare-and-swap configuration update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigApply {
    /// Revision observed by the caller; zero before the first apply.
    pub base_revision: u64,
    /// Resolved snapshot to validate and commit.
    pub config: ResolvedConfig,
}
/// Active configuration snapshot (secrets are never returned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    /// Monotonically increasing successful-apply revision.
    pub revision: u64,
    /// None before first apply.
    pub config: Option<ResolvedConfig>,
}
/// Session command routing parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    /// Target conversation.
    pub session_id: SessionId,
}
/// Accept a user message and start a run in that session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStart {
    /// Target session.
    pub session_id: SessionId,
    /// User input; never logged by default.
    pub message: String,
}
/// Cancel exactly the named run, not a future run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCancel {
    /// Target session.
    pub session_id: SessionId,
    /// Target active run.
    pub run_id: RunId,
}
/// Replay and live subscription request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscribe {
    /// Target session.
    pub session_id: SessionId,
    /// Last canonical event observed; zero replays everything retained.
    pub after_seq: u64,
}
/// Model-facing capability; execution topology is separate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Name unique within a session.
    pub name: String,
    /// Description supplied to the model.
    pub description: String,
    /// JSON Schema object for arguments.
    pub input_schema: Value,
}
/// Register connection-lifetime tools scoped to a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsRegister {
    /// Target session.
    pub session_id: SessionId,
    /// Capabilities implemented by the registering Client.
    pub tools: Vec<ToolDefinition>,
}
/// Server-issued authority for one hosted tool attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLease {
    /// Logical tool invocation.
    pub tool_run_id: ToolRunId,
    /// Selected side-effect owner.
    pub executor_id: ExecutorId,
    /// Fences results from obsolete assignments.
    pub generation: u64,
}
/// Reverse request sent to the executor Client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecute {
    /// Conversation scope used to resolve the registered capability.
    pub session_id: SessionId,
    /// Server-issued authority.
    pub lease: ToolLease,
    /// Registered capability name.
    pub name: String,
    /// Model-supplied arguments; not tracing fields.
    pub arguments: Value,
}
/// Tool result returned by an executor. Its lease must match exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Echoed execution authority.
    pub lease: ToolLease,
    /// JSON result; sensitive and never logged by default.
    pub output: Value,
}
/// Canonical committed session transition, replayed unchanged after reconnect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Owning conversation.
    pub session_id: SessionId,
    /// Contiguous sequence assigned by the session actor at commit.
    pub seq: u64,
    /// What happened, distinct from provider step outcomes.
    #[serde(flatten)]
    pub kind: EventKind,
}
/// Observable transitions of the initial live state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// Session has been committed.
    SessionCreated,
    /// User message was accepted.
    MessageAccepted {
        /// Accepted content.
        message: String,
    },
    /// Run authority was created.
    RunStarted {
        /// New run.
        run_id: RunId,
        /// Configuration pinned for this run.
        config_revision: u64,
    },
    /// Server authorized an inference step.
    ModelCallStarted {
        /// Owning run.
        run_id: RunId,
        /// Inference identity.
        model_call_id: ModelCallId,
    },
    /// Hosted tool was assigned an execution lease.
    ToolStarted {
        /// Owning run.
        run_id: RunId,
        /// Execution authority.
        lease: ToolLease,
        /// Capability name.
        name: String,
    },
    /// A matching tool result was committed.
    ToolCompleted {
        /// Owning run.
        run_id: RunId,
        /// Completed authority.
        lease: ToolLease,
    },
    /// Final assistant content was committed.
    AssistantMessage {
        /// Owning run.
        run_id: RunId,
        /// Assistant content.
        text: String,
    },
    /// Terminal success.
    RunCompleted {
        /// Completed run.
        run_id: RunId,
    },
    /// Terminal failure (no raw provider or tool error data).
    RunFailed {
        /// Failed run.
        run_id: RunId,
        /// Structured reason.
        error: ProtocolError,
    },
    /// Cancellation committed before outstanding work was aborted.
    RunCancelled {
        /// Cancelled run.
        run_id: RunId,
    },
}
impl EventKind {
    /// Whether this event terminates the indicated run.
    pub fn terminal_run(&self) -> Option<RunId> {
        match self {
            Self::RunCompleted { run_id }
            | Self::RunFailed { run_id, .. }
            | Self::RunCancelled { run_id } => Some(*run_id),
            _ => None,
        }
    }
}
