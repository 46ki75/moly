//! Static failure categories; raw HTTP, JWT, I/O, and peer diagnostics never cross STDIO.

use moly_protocol::ProtocolError;

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum Error {
    #[error("Invalid provider parameters")]
    InvalidParams,
    #[error("Invalid Codex provider configuration")]
    InvalidConfig,
    #[error("Sign in with ChatGPT is required")]
    AuthRequired,
    #[error("Sign-in interaction was declined or unavailable")]
    Interaction,
    #[error("OAuth callback could not be verified")]
    Callback,
    #[error("OpenID Connect identity could not be verified")]
    Identity,
    #[error("ChatGPT plan usage permission was not granted")]
    Scope,
    #[error("OAuth client registration is invalid")]
    Registration,
    #[error("Provider network operation failed")]
    Network,
    #[error("Provider operation timed out")]
    Timeout,
    #[error("Provider data exceeds a size limit")]
    TooLarge,
    #[error("Invalid Responses API result")]
    InvalidResponse,
    #[error("Unsupported Responses API outcome")]
    Unsupported,
    #[error("ChatGPT plan usage limit reached")]
    UsageLimit,
    #[error("ChatGPT plan usage is temporarily unavailable")]
    UsageUnavailable,
    #[error("ChatGPT plan usage was denied")]
    PermissionDenied,
    #[error("Responses API inference failed")]
    ProviderFailed,
    #[error("Responses API inference was incomplete")]
    Incomplete,
    #[error("Responses API stream ended without completion")]
    Interrupted,
    #[error("Invalid or inconsistent Responses replay metadata")]
    Replay,
    #[error("Provider host protocol failed")]
    Host,
    #[error("Provider internal operation failed")]
    Internal,
}

impl Error {
    pub(crate) fn protocol(self) -> ProtocolError {
        let code = match self {
            Self::InvalidParams => "invalid_params",
            Self::InvalidConfig => "invalid_config",
            Self::AuthRequired => "auth_required",
            Self::Interaction => "auth_interaction_unavailable",
            Self::Callback | Self::Identity => "auth_failed",
            Self::Scope => "auth_scope_required",
            Self::Registration => "auth_invalid_client",
            Self::Network => "provider_unavailable",
            Self::Timeout => "provider_timeout",
            Self::TooLarge => "provider_response_too_large",
            Self::InvalidResponse => "provider_protocol",
            Self::Unsupported => "provider_unsupported",
            Self::UsageLimit => "provider_usage_limit",
            Self::UsageUnavailable => "provider_usage_unavailable",
            Self::PermissionDenied => "provider_permission_denied",
            Self::Incomplete => "provider_incomplete",
            Self::Interrupted => "provider_interrupted",
            Self::Replay => "provider_replay_invalid",
            Self::Host => "provider_protocol",
            Self::ProviderFailed | Self::Internal => "provider_error",
        };
        ProtocolError::new(code, &self.to_string())
    }
}
