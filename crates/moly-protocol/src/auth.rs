//! Experimental interactive authentication and scoped Provider host services.
use crate::AuthAttemptId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An explicit credential operation, separate from model inference and validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum AuthOperation {
    /// Authenticate using host-provided interaction when necessary.
    Login,
    /// Inspect local credential state without a network authentication check.
    Status,
    /// Stop using credentials and attempt upstream revocation.
    Logout,
}

// Serde's default unit-enum decoder also accepts {"login": null}. The
// language-neutral protocol requires strings, not externally tagged objects.
impl TryFrom<String> for AuthOperation {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "login" => Ok(Self::Login),
            "status" => Ok(Self::Status),
            "logout" => Ok(Self::Logout),
            _ => Err("unknown authentication operation"),
        }
    }
}

/// Client request pinned to the active resolved configuration revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCommand {
    /// Fresh identity, never reused on this Client connection.
    pub attempt_id: AuthAttemptId,
    /// Operation to perform using the configured Provider.
    pub operation: AuthOperation,
    /// Reject stale configuration before launching the Provider.
    pub config_revision: u64,
}

/// Cancel one authentication operation belonging to this connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCancel {
    /// Identity from the original command, not a run or process ID.
    pub attempt_id: AuthAttemptId,
}

/// Private MPP host-to-Provider authentication payload. Never log or debug it.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderAuthRequest {
    /// Host-accepted operation identity.
    pub attempt_id: AuthAttemptId,
    /// Explicit action; validation never authenticates.
    pub operation: AuthOperation,
    /// Opaque resolved configuration interpreted only by the implementation.
    pub options: Value,
    /// Selected opaque credential record, not model context or configuration.
    pub credential: Option<String>,
}

/// Nonsecret authentication result. This is not proof of model entitlement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatus {
    /// Echoes the accepted operation identity.
    pub attempt_id: AuthAttemptId,
    /// Whether a locally usable credential record is present.
    pub authenticated: bool,
    /// Nonsecret, Provider-defined registration data for application-owned persistence.
    pub registration: Option<Value>,
    /// Logout only: whether upstream revocation was confirmed.
    pub revocation_confirmed: Option<bool>,
}

/// Reverse interaction for the initiating host; Servers route it only to the initiating Client.
/// URLs can contain sensitive login hints: never put this payload in logs/history.
#[derive(Clone, Serialize, Deserialize)]
pub struct InteractionRequest {
    /// Binds this interaction to one live authentication operation.
    pub attempt_id: AuthAttemptId,
    /// HTTPS system-browser URL; no shell commands or arbitrary UI programs.
    pub url: String,
}

/// The host's presentation outcome, not authentication verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum InteractionOutcome {
    /// Presented for opening in a browser; Provider must still verify its callback.
    Opened,
    /// User declined the requested interaction.
    Declined,
    /// This host cannot present a browser interaction.
    Unavailable,
}

impl TryFrom<String> for InteractionOutcome {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "opened" => Ok(Self::Opened),
            "declined" => Ok(Self::Declined),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err("unknown interaction outcome"),
        }
    }
}

/// Correlated answer to a reverse interaction request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionResponse {
    /// Must match the pending request exactly.
    pub attempt_id: AuthAttemptId,
    /// Presentation outcome; never an access token or authorization code.
    pub outcome: InteractionOutcome,
}

/// Replace the credential selected by the current operation; no arbitrary key access.
/// The host serializes all operations using that credential. Never log this payload.
#[derive(Clone, Serialize, Deserialize)]
pub struct CredentialReplace {
    /// New opaque record, or null to clear it. Maximum 64 KiB.
    pub credential: Option<String>,
}
