//! Nonsecret registration mapping and explicit, network-free configuration checks.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::Error;

pub(crate) const ISSUER: &str = "https://auth.openai.com";
pub(crate) const RESOURCE: &str = "https://api.openai.com/v1";
pub(crate) const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
pub(crate) const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Registration {
    pub(crate) version: u8,
    pub(crate) client_id: String,
    pub(crate) subject: String,
    pub(crate) issuer: String,
    pub(crate) host_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Options {
    pub(crate) model: String,
    pub(crate) host_id: String,
    pub(crate) registration: Option<Registration>,
}

pub(crate) fn bounded_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

pub(crate) fn issued_client(value: &str) -> bool {
    value.starts_with("oaiapp_")
        && value.len() > 7
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn host_id(value: &str) -> bool {
    let Some(raw) = value.strip_prefix("urn:uuid:") else {
        return false;
    };
    let Ok(uuid) = Uuid::parse_str(raw) else {
        return false;
    };
    uuid.get_version_num() == 4
        && uuid.get_variant() == uuid::Variant::RFC4122
        && value == format!("urn:uuid:{uuid}")
}

impl Registration {
    pub(crate) fn validate(&self, host: &str, issuer: &str) -> Result<(), Error> {
        if self.version != 1
            || !issued_client(&self.client_id)
            || !bounded_text(&self.subject, 255)
            || !self.subject.is_ascii()
            || self.issuer != issuer
            || self.host_id != host
            || !host_id(host)
        {
            return Err(Error::Registration);
        }
        Ok(())
    }
}

impl Options {
    pub(crate) fn parse(value: Value) -> Result<Self, Error> {
        if !value.is_object() {
            return Err(Error::InvalidConfig);
        }
        let options: Self = serde_json::from_value(value).map_err(|_| Error::InvalidConfig)?;
        if !bounded_text(&options.model, 256) || !host_id(&options.host_id) {
            return Err(Error::InvalidConfig);
        }
        if let Some(registration) = &options.registration {
            registration
                .validate(&options.host_id, ISSUER)
                .map_err(|_| Error::InvalidConfig)?;
        }
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn options_are_explicit_and_never_endpoint_or_token_configuration() {
        let valid =
            json!({"model":"selected", "host_id":"urn:uuid:00000000-0000-4000-8000-000000000001"});
        assert!(Options::parse(valid.clone()).is_ok());
        for field in ["model_endpoint", "token", "access_token"] {
            let mut value = valid.clone();
            value[field] = json!("https://untrusted.invalid");
            assert!(Options::parse(value).is_err());
        }
        for host in [
            "urn:uuid:00000000-0000-1000-8000-000000000001",
            "urn:uuid:00000000-0000-4000-0000-000000000001",
            "localhost",
        ] {
            let mut value = valid.clone();
            value["host_id"] = json!(host);
            assert!(Options::parse(value).is_err());
        }
        let mut value = valid;
        value["model"] = json!("  ");
        assert!(Options::parse(value).is_err());
    }
}
