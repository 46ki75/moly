//! Direct SIWC public-client OAuth; no legacy Codex IDs, auth.json, or backend-api.
//!
//! Sources: https://developers.openai.com/siwc/token-sharing-open-source/sign-in.md
//! https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions.md
//! https://developers.openai.com/siwc/token-sharing-open-source/token-reference.md
//! https://developers.openai.com/siwc/website.md

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use moly_protocol::AuthAttemptId;
use moly_protocol::auth::{InteractionOutcome, InteractionRequest};
use rand::RngCore;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::{
    DYNAMIC_CLIENT, ISSUER, Options, RESOURCE, Registration, SCOPES, bounded_text, issued_client,
};
use crate::error::Error;
use crate::http;
use crate::transport::Host;

const AUTHORITY: &str = "auth.openai.com";
const MAX_JSON: usize = 64 * 1024;
const REQUIRED_SCOPES: [&str; 3] = ["openid", "resource.invoke", "chatgpt.tokens.use.direct"];
const REFRESH_LIFETIME: u64 = 30 * 24 * 60 * 60;

#[derive(Clone)]
pub(crate) struct Endpoints {
    pub(crate) discovery: String,
    pub(crate) responses: String,
    // Tests may only trust the exact loopback authority chosen by their own listener.
    #[cfg(test)]
    pub(crate) test_origin: Option<String>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            discovery: format!("{ISSUER}/.well-known/openid-configuration"),
            responses: format!("{RESOURCE}/responses"),
            #[cfg(test)]
            test_origin: None,
        }
    }
}

impl Endpoints {
    fn check_auth_endpoint(&self, endpoint: &str) -> Result<(), Error> {
        let url = Url::parse(endpoint).map_err(|_| Error::Identity)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
            || endpoint.chars().any(char::is_control)
        {
            return Err(Error::Identity);
        }
        if url.scheme() == "https"
            && url.host_str() == Some(AUTHORITY)
            && url.port_or_known_default() == Some(443)
        {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(origin) = &self.test_origin
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.origin().ascii_serialization() == *origin
        {
            return Ok(());
        }
        Err(Error::Identity)
    }
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: Option<String>,
    id_token_signing_alg_values_supported: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    version: u8,
    pub(crate) registration: Registration,
    pub(crate) access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    oidc: OidcBinding,
    scopes: Vec<String>,
    expires_at: u64,
    refresh_expires_at: Option<u64>,
    saved_at: u64,
    earliest_refresh_at: Option<u64>,
}

impl Credential {
    pub(crate) fn load(raw: Option<&str>, options: &Options) -> Result<Option<Self>, Error> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        if raw.len() > MAX_JSON {
            return Err(Error::AuthRequired);
        }
        let credential: Self = serde_json::from_str(raw).map_err(|_| Error::AuthRequired)?;
        credential
            .registration
            .validate(&options.host_id, ISSUER)
            .map_err(|_| Error::AuthRequired)?;
        if credential.version != 1
            || !token(&credential.access_token)
            || !token(&credential.id_token)
            || !bounded_text(&credential.oidc.nonce, 256)
            || !audience_contains(
                &credential.oidc.audience,
                &credential.registration.client_id,
            )
            || credential
                .refresh_token
                .as_ref()
                .is_some_and(|value| !token(value))
            || credential.expires_at <= credential.saved_at
            || credential.refresh_token.is_some() != credential.refresh_expires_at.is_some()
            || credential
                .refresh_expires_at
                .is_some_and(|expiry| expiry <= credential.saved_at)
            || credential.scopes.len() > 64
            || credential.scopes.iter().any(|scope| !scope_text(scope))
            || options
                .registration
                .as_ref()
                .is_some_and(|registration| *registration != credential.registration)
        {
            return Err(Error::AuthRequired);
        }
        Ok(Some(credential))
    }

    pub(crate) fn serialize(&self) -> Result<String, Error> {
        let raw = serde_json::to_string(self).map_err(|_| Error::Internal)?;
        if raw.len() > MAX_JSON {
            return Err(Error::TooLarge);
        }
        Ok(raw)
    }

    pub(crate) fn usable(&self, now: u64) -> bool {
        has_scopes(&self.scopes)
            && (self.expires_at > now || self.refresh_expires_at.is_some_and(|expiry| expiry > now))
    }

    pub(crate) fn access_valid(&self, now: u64) -> bool {
        has_scopes(&self.scopes) && self.expires_at > now
    }
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16 * 1024
        && value.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
}
fn scope_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
}
fn has_scopes(scopes: &[String]) -> bool {
    REQUIRED_SCOPES
        .iter()
        .all(|required| scopes.iter().any(|scope| scope == required))
}

pub(crate) fn now() -> Result<u64, Error> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Internal)?
        .as_secs())
}

pub(crate) struct OAuth {
    pub(crate) client: Client,
    pub(crate) endpoints: Endpoints,
}

impl OAuth {
    pub(crate) fn new() -> Result<Self, Error> {
        Ok(Self {
            client: http::client()?,
            endpoints: Endpoints::default(),
        })
    }

    async fn discovery(&self) -> Result<Discovery, Error> {
        self.endpoints
            .check_auth_endpoint(&self.endpoints.discovery)?;
        let response = self
            .client
            .get(&self.endpoints.discovery)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(http::network)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::Network);
        }
        let discovery: Discovery = http::json(response, MAX_JSON).await?;
        if discovery.issuer != ISSUER {
            return Err(Error::Identity);
        }
        for endpoint in [
            &discovery.authorization_endpoint,
            &discovery.token_endpoint,
            &discovery.jwks_uri,
        ] {
            self.endpoints.check_auth_endpoint(endpoint)?;
        }
        // Discovery cannot silently move this public-client flow to another endpoint.
        if discovery.authorization_endpoint != format!("{ISSUER}/api/accounts/authorize") {
            return Err(Error::Identity);
        }
        #[cfg(not(test))]
        if discovery.token_endpoint != format!("{ISSUER}/api/accounts/oauth/token") {
            return Err(Error::Identity);
        }
        Ok(discovery)
    }

    pub(crate) async fn login(
        &self,
        options: &Options,
        existing: Option<&Credential>,
        attempt: AuthAttemptId,
        host: &Host,
    ) -> Result<Credential, Error> {
        let discovery = self.discovery().await?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| Error::Network)?;
        let address = listener.local_addr().map_err(|_| Error::Network)?;
        let pending = Pending::new(format!("http://127.0.0.1:{}/auth/callback", address.port()))?;
        let registration = options
            .registration
            .as_ref()
            .or_else(|| existing.map(|value| &value.registration));
        let url = pending.authorize(&discovery.authorization_endpoint, options, registration)?;
        let interaction = host
            .interact(InteractionRequest {
                attempt_id: attempt,
                url,
            })
            .await?;
        if interaction.attempt_id != attempt {
            return Err(Error::Host);
        }
        if interaction.outcome != InteractionOutcome::Opened {
            return Err(Error::Interaction);
        }
        let callback = receive_callback(&listener, &pending, registration).await?;
        let response = self
            .client
            .post(&discovery.token_endpoint)
            .timeout(Duration::from_secs(15))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", callback.client_id.as_str()),
                ("code", callback.code.as_str()),
                ("code_verifier", pending.verifier.as_str()),
                ("redirect_uri", pending.redirect.as_str()),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(http::network)?;
        let tokens = read_tokens(response).await?;
        let identity = self
            .verify(
                &discovery,
                &tokens.id_token,
                &callback.client_id,
                Some(&pending.nonce),
                now()?,
            )
            .await?;
        identity.verify_access_hash(&tokens.access_token)?;
        if registration.is_some_and(|selected| selected.subject != identity.sub) {
            return Err(Error::Identity);
        }
        let registration = Registration {
            version: 1,
            client_id: callback.client_id,
            subject: identity.sub.clone(),
            issuer: ISSUER.into(),
            host_id: options.host_id.clone(),
        };
        credential(tokens, registration, now()?, None, Some(&identity))
    }

    async fn verify(
        &self,
        discovery: &Discovery,
        id_token: &str,
        client_id: &str,
        nonce: Option<&str>,
        now: u64,
    ) -> Result<Claims, Error> {
        let response = self
            .client
            .get(&discovery.jwks_uri)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(http::network)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::Network);
        }
        let keys: jsonwebtoken::jwk::JwkSet = http::json(response, MAX_JSON).await?;
        verify_identity(
            id_token,
            &keys,
            &discovery.id_token_signing_alg_values_supported,
            client_id,
            nonce,
            now,
        )
    }

    /// Returns a replacement only after rotation and identity validation have finished.
    /// The caller must commit it through the host before using its access token.
    pub(crate) async fn refresh(&self, previous: &Credential) -> Result<Credential, Error> {
        let now = now()?;
        let refresh = previous
            .refresh_token
            .as_deref()
            .ok_or(Error::AuthRequired)?;
        if previous
            .refresh_expires_at
            .is_none_or(|expiry| expiry <= now)
        {
            return Err(Error::AuthRequired);
        }
        if previous
            .earliest_refresh_at
            .is_some_and(|earliest| earliest > now)
        {
            return Err(Error::AuthRequired);
        }
        let discovery = self.discovery().await?;
        let response = self
            .client
            .post(&discovery.token_endpoint)
            .timeout(Duration::from_secs(15))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", previous.registration.client_id.as_str()),
                ("refresh_token", refresh),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(http::network)?;
        let tokens = read_tokens(response).await?;
        // OIDC Core 12.2 permits omitting the ID token and its nonce on refresh,
        // but any returned nonce/audience/auth_time must retain the original binding.
        // https://openid.net/specs/openid-connect-core-1_0.html#RefreshTokenResponse
        if !tokens.id_token.is_empty() {
            let identity = self
                .verify(
                    &discovery,
                    &tokens.id_token,
                    &previous.registration.client_id,
                    None,
                    now,
                )
                .await?;
            identity.verify_access_hash(&tokens.access_token)?;
            if identity.sub != previous.registration.subject {
                return Err(Error::Identity);
            }
            previous.oidc.verify_refresh(&identity)?;
        }
        credential(
            tokens,
            previous.registration.clone(),
            now,
            Some(previous),
            None,
        )
    }

    pub(crate) fn needs_refresh(credential: &Credential, now: u64) -> bool {
        credential.expires_at <= now.saturating_add(60)
            && credential
                .earliest_refresh_at
                .is_none_or(|earliest| earliest <= now)
    }

    /// Remote revocation is best effort and has no inference/refresh retry machinery.
    /// Local clearing must happen regardless of this result.
    pub(crate) async fn revoke(&self, credential: &Credential) -> bool {
        let Some(refresh) = credential.refresh_token.as_deref() else {
            return false;
        };
        let result = async {
            let discovery = self.discovery().await?;
            let endpoint = discovery.revocation_endpoint.ok_or(Error::Network)?;
            self.endpoints.check_auth_endpoint(&endpoint)?;
            let response = self
                .client
                .post(endpoint)
                .timeout(Duration::from_secs(10))
                .form(&[
                    ("token", refresh),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", credential.registration.client_id.as_str()),
                ])
                .send()
                .await
                .map_err(http::network)?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(Error::Network);
            }
            // The documented success is an empty 200, not an arbitrary JSON body.
            if !http::body(response, 1024).await?.is_empty() {
                return Err(Error::Network);
            }
            Ok::<_, Error>(())
        };
        tokio::time::timeout(Duration::from_secs(22), result)
            .await
            .is_ok_and(|result| result.is_ok())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: String,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    earliest_refresh_at: Option<u64>,
}

async fn read_tokens(response: reqwest::Response) -> Result<TokenResponse, Error> {
    let success = response.status() == reqwest::StatusCode::OK;
    let raw = http::body(response, MAX_JSON).await?;
    if !success {
        let body: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| Error::Network)?;
        return Err(match body.get("error").and_then(|value| value.as_str()) {
            Some(
                "invalid_grant"
                | "invalid_refresh_token"
                | "token_expired"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
                | "refresh_token_reused",
            ) => Error::AuthRequired,
            Some("invalid_client") => Error::Registration,
            _ => Error::Network,
        });
    }
    serde_json::from_slice(&raw).map_err(|_| Error::Identity)
}

fn credential(
    tokens: TokenResponse,
    registration: Registration,
    now: u64,
    previous: Option<&Credential>,
    identity: Option<&Claims>,
) -> Result<Credential, Error> {
    if !tokens.token_type.eq_ignore_ascii_case("Bearer")
        || !token(&tokens.access_token)
        || !(1..=24 * 60 * 60).contains(&tokens.expires_in)
        || tokens
            .refresh_token
            .as_ref()
            .is_some_and(|value| !token(value))
        || (!tokens.id_token.is_empty() && !token(&tokens.id_token))
    {
        return Err(Error::Identity);
    }
    let scopes: Vec<String> = match tokens.scope {
        Some(scopes) => scopes.split_ascii_whitespace().map(str::to_owned).collect(),
        None => previous
            .map(|previous| previous.scopes.clone())
            .ok_or(Error::Scope)?,
    };
    if scopes.len() > 64 || scopes.iter().any(|scope| !scope_text(scope)) || !has_scopes(&scopes) {
        return Err(Error::Scope);
    }
    // SIWC promises replacement refresh tokens. Accepting an omitted replacement
    // could retain a now-consumed rotating token and make the record inconsistent.
    if tokens.refresh_token.is_none()
        && (previous.is_some() || scopes.iter().any(|scope| scope == "offline_access"))
    {
        return Err(Error::Identity);
    }
    let id_token = if tokens.id_token.is_empty() {
        previous
            .map(|previous| previous.id_token.clone())
            .ok_or(Error::Identity)?
    } else {
        tokens.id_token
    };
    Ok(Credential {
        version: 1,
        registration,
        access_token: tokens.access_token,
        refresh_expires_at: tokens
            .refresh_token
            .as_ref()
            .map(|_| now.saturating_add(REFRESH_LIFETIME)),
        refresh_token: tokens.refresh_token,
        id_token,
        oidc: match previous {
            Some(previous) => previous.oidc.clone(),
            None => OidcBinding::new(identity.ok_or(Error::Identity)?)?,
        },
        scopes,
        expires_at: now.checked_add(tokens.expires_in).ok_or(Error::Identity)?,
        saved_at: now,
        earliest_refresh_at: tokens.earliest_refresh_at,
    })
}

struct Pending {
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
}

fn random() -> Result<String, Error> {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| Error::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

impl Pending {
    fn new(redirect: String) -> Result<Self, Error> {
        Ok(Self {
            state: random()?,
            nonce: random()?,
            verifier: random()?,
            redirect,
        })
    }

    fn authorize(
        &self,
        endpoint: &str,
        options: &Options,
        registration: Option<&Registration>,
    ) -> Result<String, Error> {
        let mut url = Url::parse(endpoint).map_err(|_| Error::Identity)?;
        url.query_pairs_mut().extend_pairs([
            (
                "client_id",
                registration.map_or(DYNAMIC_CLIENT, |value| value.client_id.as_str()),
            ),
            ("ext_agent_host_id", options.host_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", self.redirect.as_str()),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", self.state.as_str()),
            ("nonce", self.nonce.as_str()),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                URL_SAFE_NO_PAD
                    .encode(Sha256::digest(self.verifier.as_bytes()))
                    .as_str(),
            ),
        ]);
        if registration.is_none() {
            url.query_pairs_mut().append_pair("agent_name_hint", "moly");
        }
        // Intentionally omit both hints: the Client may present this URL, and it
        // must contain no ID token or account email. Returning identity is verified.
        let url: String = url.into();
        if url.len() > 8192 || url.chars().any(char::is_control) {
            return Err(Error::TooLarge);
        }
        Ok(url)
    }

    fn callback(
        &self,
        target: &str,
        registration: Option<&Registration>,
    ) -> Result<Callback, Error> {
        if target.len() > 8192
            || !target.starts_with("/auth/callback?")
            || target.chars().any(char::is_control)
        {
            return Err(Error::Callback);
        }
        let base = Url::parse(&self.redirect).map_err(|_| Error::Callback)?;
        let url = base.join(target).map_err(|_| Error::Callback)?;
        if url.path() != "/auth/callback" || url.fragment().is_some() {
            return Err(Error::Callback);
        }
        let mut params = BTreeMap::new();
        for (key, value) in url.query_pairs() {
            if params
                .insert(key.into_owned(), value.into_owned())
                .is_some()
            {
                return Err(Error::Callback);
            }
        }
        let state = params.get("state").ok_or(Error::Callback)?;
        if !bool::from(state.as_bytes().ct_eq(self.state.as_bytes())) {
            return Err(Error::Callback);
        }
        if params.contains_key("error") {
            return Err(Error::Interaction);
        }
        let code = params
            .remove("code")
            .filter(|value| bounded_text(value, 4096))
            .ok_or(Error::Callback)?;
        let client_id = match (registration, params.remove("client_id")) {
            (Some(selected), Some(returned)) if returned == selected.client_id => returned,
            (Some(selected), None) => selected.client_id.clone(),
            (None, Some(returned)) if issued_client(&returned) => returned,
            _ => return Err(Error::Registration),
        };
        Ok(Callback { code, client_id })
    }
}

struct Callback {
    code: String,
    client_id: String,
}

async fn receive_callback(
    listener: &TcpListener,
    pending: &Pending,
    registration: Option<&Registration>,
) -> Result<Callback, Error> {
    // Accept a finite number of unrelated local connections. Each header has its
    // own small limit/deadline; the overall auth deadline cancels accept as well.
    for _ in 0..8 {
        let (mut stream, peer) = listener.accept().await.map_err(|_| Error::Network)?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let request = tokio::time::timeout(Duration::from_secs(3), async {
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream
                    .read(&mut buffer)
                    .await
                    .map_err(|_| Error::Callback)?;
                if count == 0 {
                    return Err(Error::Callback);
                }
                if count > 16 * 1024 - bytes.len() {
                    return Err(Error::Callback);
                }
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            String::from_utf8(bytes).map_err(|_| Error::Callback)
        })
        .await
        .map_err(|_| Error::Timeout)?;
        let parsed = request.and_then(|request| {
            let mut lines = request.split("\r\n");
            let line = lines.next().ok_or(Error::Callback)?;
            let mut parts = line.split(' ');
            if parts.next() != Some("GET") {
                return Err(Error::Callback);
            }
            let target = parts.next().ok_or(Error::Callback)?;
            if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
                return Err(Error::Callback);
            }
            let expected_host = Url::parse(&pending.redirect).map_err(|_| Error::Callback)?;
            let authority = format!("127.0.0.1:{}", expected_host.port().ok_or(Error::Callback)?);
            let hosts: Vec<_> = lines
                .filter_map(|line| line.split_once(':'))
                .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
                .collect();
            if hosts.len() != 1 || hosts[0].1.trim() != authority {
                return Err(Error::Callback);
            }
            pending.callback(target, registration)
        });
        let success = parsed.is_ok();
        let body = if success {
            "Sign-in received. You may close this tab."
        } else {
            "Sign-in could not be verified. Return to moly."
        };
        let response = format!(
            "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            if success { "200 OK" } else { "400 Bad Request" },
            body.len(),
            body
        );
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            stream.write_all(response.as_bytes()),
        )
        .await;
        // A callback with failed state is terminal; do not redeem or keep waiting
        // after an attacker-controlled attempt. No callback values are echoed.
        return parsed;
    }
    Err(Error::Callback)
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
    iat: u64,
    nonce: Option<String>,
    azp: Option<String>,
    aud: serde_json::Value,
    nbf: Option<u64>,
    at_hash: Option<String>,
    auth_time: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OidcBinding {
    audience: serde_json::Value,
    nonce: String,
    auth_time: Option<u64>,
}

fn audience_contains(audience: &serde_json::Value, client: &str) -> bool {
    match audience {
        serde_json::Value::String(audience) => audience == client,
        serde_json::Value::Array(audiences) => {
            !audiences.is_empty()
                && audiences
                    .iter()
                    .all(|value| value.as_str().is_some_and(|value| bounded_text(value, 256)))
                && audiences.iter().any(|value| value.as_str() == Some(client))
        }
        _ => false,
    }
}

impl OidcBinding {
    fn new(identity: &Claims) -> Result<Self, Error> {
        Ok(Self {
            audience: identity.aud.clone(),
            nonce: identity.nonce.clone().ok_or(Error::Identity)?,
            auth_time: identity.auth_time,
        })
    }

    fn verify_refresh(&self, identity: &Claims) -> Result<(), Error> {
        if identity.aud != self.audience
            || identity
                .nonce
                .as_deref()
                .is_some_and(|nonce| !bool::from(nonce.as_bytes().ct_eq(self.nonce.as_bytes())))
            || identity
                .auth_time
                .is_some_and(|time| Some(time) != self.auth_time)
        {
            return Err(Error::Identity);
        }
        Ok(())
    }
}

impl Claims {
    fn verify_access_hash(&self, access_token: &str) -> Result<(), Error> {
        if let Some(expected) = &self.at_hash {
            // RS256 and ES256 both use the left half of SHA-256 for OIDC at_hash.
            let digest = Sha256::digest(access_token.as_bytes());
            let actual = URL_SAFE_NO_PAD.encode(&digest[..16]);
            if !bool::from(actual.as_bytes().ct_eq(expected.as_bytes())) {
                return Err(Error::Identity);
            }
        }
        Ok(())
    }
}

fn verify_identity(
    raw: &str,
    keys: &jsonwebtoken::jwk::JwkSet,
    advertised: &[String],
    client_id: &str,
    nonce: Option<&str>,
    now: u64,
) -> Result<Claims, Error> {
    if !token(raw) {
        return Err(Error::Identity);
    }
    let header = decode_header(raw).map_err(|_| Error::Identity)?;
    let algorithm = match header.alg {
        Algorithm::RS256 => "RS256",
        Algorithm::ES256 => "ES256",
        _ => return Err(Error::Identity),
    };
    if !advertised.iter().any(|value| value == algorithm) {
        return Err(Error::Identity);
    }
    let kid = header.kid.ok_or(Error::Identity)?;
    if !bounded_text(&kid, 256) || keys.keys.len() > 32 {
        return Err(Error::Identity);
    }
    let matching: Vec<_> = keys
        .keys
        .iter()
        .filter(|key| key.common.key_id.as_deref() == Some(kid.as_str()))
        .collect();
    if matching.len() != 1 {
        return Err(Error::Identity);
    }
    let key = matching[0];
    if key
        .common
        .public_key_use
        .as_ref()
        .is_some_and(|usage| *usage != jsonwebtoken::jwk::PublicKeyUse::Signature)
        || key
            .common
            .key_operations
            .as_ref()
            .is_some_and(|operations| {
                !operations.contains(&jsonwebtoken::jwk::KeyOperations::Verify)
            })
        || key
            .common
            .key_algorithm
            .as_ref()
            .is_some_and(|alg| format!("{alg:?}") != algorithm)
    {
        return Err(Error::Identity);
    }
    let key = DecodingKey::from_jwk(key).map_err(|_| Error::Identity)?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iat", "sub", "iss", "aud"]);
    // Signature/issuer/audience checks stay in the maintained crypto library.
    // Time checks use the supplied clock so tests do not need global clock overrides.
    validation.validate_exp = false;
    validation.validate_nbf = false;
    let claims = decode::<Claims>(raw, &key, &validation)
        .map_err(|_| Error::Identity)?
        .claims;
    if !bounded_text(&claims.sub, 255)
        || !claims.sub.is_ascii()
        || !audience_contains(&claims.aud, client_id)
        || claims
            .auth_time
            .is_some_and(|time| time > now.saturating_add(5))
        || claims.exp <= now
        || claims.exp <= claims.iat
        || claims.iat > now.saturating_add(5)
        || claims.nbf.is_some_and(|nbf| nbf > now.saturating_add(5))
        || nonce.is_some_and(|expected| {
            claims
                .nonce
                .as_deref()
                .is_none_or(|actual| !bool::from(actual.as_bytes().ct_eq(expected.as_bytes())))
        })
        || claims.azp.as_deref().is_some_and(|azp| azp != client_id)
        || claims.aud.as_array().is_some_and(|audiences| {
            audiences.len() > 1 && claims.azp.as_deref() != Some(client_id)
        })
    {
        return Err(Error::Identity);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod interaction_tests;
