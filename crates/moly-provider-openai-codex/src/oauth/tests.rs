use std::sync::{Arc, Mutex};

use moly_protocol::auth::AuthOperation;
use moly_protocol::{AuthAttemptId, Body};
use serde_json::{Value, json};

use super::*;
use crate::service::Service;
use crate::test_support::{self as fixture, Peer, Reply, Server, TestResult};

pub(super) fn registered() -> Registration {
    Registration {
        version: 1,
        client_id: "oaiapp_local".into(),
        subject: "local-account".into(),
        issuer: ISSUER.into(),
        host_id: "urn:uuid:00000000-0000-4000-8000-000000000001".into(),
    }
}

fn saved(time: u64) -> Result<Credential, Box<dyn std::error::Error + Send + Sync>> {
    let tokens: TokenResponse = serde_json::from_value(
        json!({"access_token":"old-access", "refresh_token":"old-refresh", "id_token":fixture::signed(&fixture::claims("old-nonce",time))?, "token_type":"Bearer", "expires_in":3600, "scope":SCOPES}),
    )?;
    let identity: Claims = serde_json::from_value(fixture::claims("old-nonce", time))?;
    Ok(credential(
        tokens,
        registered(),
        time,
        None,
        Some(&identity),
    )?)
}

#[test]
fn local_rsa_oidc_positive_and_claim_negatives() -> TestResult {
    let now = 2_000_000_000;
    let keys = serde_json::from_value(fixture::jwks()?)?;
    let algorithms = vec!["RS256".into()];
    let good = fixture::claims("expected-nonce", now);
    let valid = fixture::signed(&good)?;
    assert_eq!(
        verify_identity(
            &valid,
            &keys,
            &algorithms,
            "oaiapp_local",
            Some("expected-nonce"),
            now
        )?
        .sub,
        "local-account"
    );
    for (field, value) in [
        ("nonce", json!("different")),
        ("iss", json!("https://evil.invalid")),
        ("aud", json!("oaiapp_wrong")),
        ("exp", json!(now - 1)),
        ("iat", json!(now + 60)),
        ("sub", json!("")),
        ("azp", json!("oaiapp_wrong")),
        ("nbf", json!(now + 60)),
    ] {
        let mut claims = good.clone();
        claims[field] = value;
        assert!(
            matches!(
                verify_identity(
                    &fixture::signed(&claims)?,
                    &keys,
                    &algorithms,
                    "oaiapp_local",
                    Some("expected-nonce"),
                    now
                ),
                Err(Error::Identity)
            ),
            "claim: {field}"
        );
    }
    for field in ["exp", "iat", "iss", "aud", "sub", "nonce"] {
        let mut claims = good.clone();
        claims.as_object_mut().ok_or("object")?.remove(field);
        assert!(
            verify_identity(
                &fixture::signed(&claims)?,
                &keys,
                &algorithms,
                "oaiapp_local",
                Some("expected-nonce"),
                now
            )
            .is_err(),
            "missing: {field}"
        );
    }
    let mut multi = good.clone();
    multi["aud"] = json!(["oaiapp_local", "another-client"]);
    assert!(
        verify_identity(
            &fixture::signed(&multi)?,
            &keys,
            &algorithms,
            "oaiapp_local",
            Some("expected-nonce"),
            now
        )
        .is_err()
    );
    multi["azp"] = json!("oaiapp_local");
    assert!(
        verify_identity(
            &fixture::signed(&multi)?,
            &keys,
            &algorithms,
            "oaiapp_local",
            Some("expected-nonce"),
            now
        )
        .is_ok()
    );
    // Alter signature bytes rather than only changing an unverified claim.
    let (prefix, signature) = valid.rsplit_once('.').ok_or("JWT signature")?;
    let mut bytes = URL_SAFE_NO_PAD.decode(signature)?;
    bytes[0] ^= 1;
    let invalid = format!("{prefix}.{}", URL_SAFE_NO_PAD.encode(bytes));
    assert!(
        verify_identity(
            &invalid,
            &keys,
            &algorithms,
            "oaiapp_local",
            Some("expected-nonce"),
            now
        )
        .is_err()
    );
    // Algorithm confusion and unadvertised keys are rejected before decoding.
    let key = jsonwebtoken::EncodingKey::from_secret(b"not-the-rsa-key");
    let mut header = jsonwebtoken::Header::new(Algorithm::HS256);
    header.kid = Some("local-key".into());
    let wrong_alg = jsonwebtoken::encode(&header, &good, &key)?;
    assert!(
        verify_identity(
            &wrong_alg,
            &keys,
            &algorithms,
            "oaiapp_local",
            Some("expected-nonce"),
            now
        )
        .is_err()
    );
    assert!(
        verify_identity(
            &valid,
            &keys,
            &[],
            "oaiapp_local",
            Some("expected-nonce"),
            now
        )
        .is_err()
    );
    let mut duplicate = fixture::jwks()?;
    duplicate["keys"]
        .as_array_mut()
        .ok_or("keys")?
        .push(fixture::jwks()?["keys"][0].clone());
    let duplicate = serde_json::from_value(duplicate)?;
    assert!(
        verify_identity(
            &valid,
            &duplicate,
            &algorithms,
            "oaiapp_local",
            Some("expected-nonce"),
            now
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn refresh_oidc_nonce_audience_and_auth_time_remain_bound_to_original_login() -> TestResult {
    let time = 2_000_000_000;
    let keys = serde_json::from_value(fixture::jwks()?)?;
    let algorithms = vec!["RS256".into()];
    let mut original = fixture::claims("original-nonce", time);
    original["auth_time"] = json!(time - 30);
    let identity = verify_identity(
        &fixture::signed(&original)?,
        &keys,
        &algorithms,
        "oaiapp_local",
        Some("original-nonce"),
        time,
    )?;
    let binding = OidcBinding::new(&identity)?;
    for (field, value) in [
        ("nonce", json!("another-transaction")),
        ("aud", json!(["oaiapp_local", "extra-audience"])),
        ("auth_time", json!(time - 20)),
    ] {
        let mut refreshed = original.clone();
        refreshed[field] = value;
        refreshed["azp"] = json!("oaiapp_local");
        let identity = verify_identity(
            &fixture::signed(&refreshed)?,
            &keys,
            &algorithms,
            "oaiapp_local",
            None,
            time,
        )?;
        assert!(
            matches!(binding.verify_refresh(&identity), Err(Error::Identity)),
            "refresh binding: {field}"
        );
    }
    original.as_object_mut().ok_or("claims")?.remove("nonce");
    let identity = verify_identity(
        &fixture::signed(&original)?,
        &keys,
        &algorithms,
        "oaiapp_local",
        None,
        time,
    )?;
    assert!(binding.verify_refresh(&identity).is_ok());
    Ok(())
}

#[test]
fn callback_state_client_binding_and_pkce_are_strict() -> TestResult {
    let options = Options::parse(fixture::options())?;
    let pending = Pending::new("http://127.0.0.1:12345/auth/callback".into())?;
    assert_ne!(pending.state, pending.nonce);
    assert_ne!(pending.state, pending.verifier);
    let url = Url::parse(&pending.authorize(
        &format!("{ISSUER}/api/accounts/authorize"),
        &options,
        None,
    )?)?;
    let params: BTreeMap<_, _> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        params.get("client_id").map(String::as_str),
        Some(DYNAMIC_CLIENT)
    );
    assert_eq!(
        params.get("agent_name_hint").map(String::as_str),
        Some("moly")
    );
    assert_eq!(params.get("resource").map(String::as_str), Some(RESOURCE));
    assert_eq!(
        params.get("code_challenge"),
        Some(&URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes())))
    );
    assert!(!params.contains_key("id_token_hint"));
    let good = format!(
        "/auth/callback?state={}&code=one-time-code&client_id=oaiapp_local",
        pending.state
    );
    assert_eq!(pending.callback(&good, None)?.client_id, "oaiapp_local");
    let registration = registered();
    assert!(
        pending
            .callback(
                &format!("/auth/callback?state={}&code=code", pending.state),
                Some(&registration)
            )
            .is_ok()
    );
    for target in [
        "/callback?state=x&code=x".to_owned(),
        "/auth/callback?state=wrong&code=x&client_id=oaiapp_local".to_owned(),
        format!(
            "/auth/callback?state={}&state={}&code=x&client_id=oaiapp_local",
            pending.state, pending.state
        ),
        format!(
            "/auth/callback?state={}&code=x&client_id=dynamic_agent_client",
            pending.state
        ),
        format!(
            "/auth/callback?state={}&code=x&client_id=oaiapp_wrong",
            pending.state
        ),
        format!("/auth/callback?state={}&error=access_denied", pending.state),
    ] {
        assert!(pending.callback(&target, Some(&registration)).is_err());
    }
    assert!(
        pending
            .callback(
                &format!("/auth/callback?state={}&code=x", pending.state),
                None
            )
            .is_err()
    );
    assert!(matches!(
        pending.callback("/auth/callback?state=wrong&error=access_denied", None),
        Err(Error::Callback)
    ));
    let url = Url::parse(&pending.authorize(
        &format!("{ISSUER}/api/accounts/authorize"),
        &options,
        Some(&registration),
    )?)?;
    assert!(
        !url.query_pairs().any(|(key, _)| key == "agent_name_hint"
            || key == "id_token_hint"
            || key == "login_hint")
    );
    Ok(())
}

#[derive(Clone, Default)]
struct LoginMode {
    bad_state: bool,
    bad_signature: bool,
    existing: bool,
    claims: Value,
    scope: Option<String>,
    returning: bool,
}

async fn login_case(
    mode: LoginMode,
) -> Result<(Server, Peer, Option<String>, Value), Box<dyn std::error::Error + Send + Sync>> {
    let transaction = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
    let for_server = transaction.clone();
    let public_keys = fixture::jwks()?;
    let token_mode = mode.clone();
    let server = Server::start(move |request, origin| match request.path.as_str() {
        "/discovery" => Reply::json(fixture::discovery(origin)),
        "/jwks" => Reply::json(public_keys.clone()),
        "/token" => {
            let form = request.form();
            assert_eq!(form.get("grant_type").map(String::as_str), Some("authorization_code"));
            assert_eq!(form.get("client_id").map(String::as_str), Some("oaiapp_local"));
            assert_eq!(form.get("resource").map(String::as_str), Some(RESOURCE));
            assert!(!form.contains_key("client_secret"));
            let verifier = form.get("code_verifier").expect("PKCE verifier");
            assert_eq!(verifier.len(),43);
            let pending = for_server.lock().expect("transaction mutex");
            assert_eq!(form.get("redirect_uri"), pending.get("redirect_uri"));
            assert_eq!(Some(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))).as_ref(), pending.get("code_challenge"));
            let mut claims = fixture::claims(pending.get("nonce").expect("OIDC nonce"), now().expect("clock"));
            if let Some(overrides) = token_mode.claims.as_object() {
                for (key,value) in overrides { claims[key] = value.clone(); }
            }
            let mut id_token = fixture::signed(&claims).expect("local signature");
            if token_mode.bad_signature {
                let (prefix, signature) = id_token.rsplit_once('.').expect("JWT signature");
                let mut bytes = URL_SAFE_NO_PAD.decode(signature).expect("base64 signature");
                bytes[0] ^= 1;
                id_token = format!("{prefix}.{}", URL_SAFE_NO_PAD.encode(bytes));
            }
            Reply::json(json!({"access_token":"local-access", "refresh_token":"local-refresh", "id_token":id_token, "token_type":"Bearer", "expires_in":3600, "scope":token_mode.scope.as_deref().unwrap_or(SCOPES)}))
        }
        "/revoke" => Reply { status:200, body:Vec::new(), content_type:"text/plain" },
        _ => Reply::error(404,"fixture-not-found"),
    }).await?;
    let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
    peer.initialize().await?;
    let attempt = AuthAttemptId::new();
    let mut options = fixture::options();
    if mode.returning {
        options["registration"] = serde_json::to_value(registered())?;
    }
    let existing = if mode.existing {
        Some(saved(now()?)?.serialize()?)
    } else {
        None
    };
    peer.request(
        2,
        "provider.auth",
        json!({"attempt_id":attempt,"operation":"login","options":options,"credential":existing}),
    )
    .await?;
    let Body::Request { id, method, params } = peer.recv().await?.body else {
        return Err("interaction request missing".into());
    };
    assert_eq!(id, 1);
    assert_eq!(method, "host.interact");
    assert_eq!(params["attempt_id"], serde_json::to_value(attempt)?);
    let authorize = Url::parse(params["url"].as_str().ok_or("authorize URL")?)?;
    assert_eq!(authorize.scheme(), "https");
    assert_eq!(authorize.host_str(), Some("auth.openai.com"));
    let query: BTreeMap<_, _> = authorize
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    *transaction.lock().map_err(|_| "transaction mutex")? = query.clone();
    let mut callback = Url::parse(query.get("redirect_uri").ok_or("redirect")?)?;
    assert_eq!(callback.host_str(), Some("127.0.0.1"));
    assert_eq!(callback.path(), "/auth/callback");
    callback
        .query_pairs_mut()
        .append_pair(
            "state",
            if mode.bad_state {
                "wrong-state"
            } else {
                query.get("state").ok_or("state")?
            },
        )
        .append_pair("code", "local-code");
    if !mode.returning {
        callback
            .query_pairs_mut()
            .append_pair("client_id", "oaiapp_local");
    }
    peer.response(id, json!({"attempt_id":attempt,"outcome":"opened"}))
        .await?;
    // This is the only browser action in the fixture: a local callback HTTP GET.
    let callback_response = crate::http::client()?.get(callback).send().await?;
    assert_eq!(
        callback_response.status().as_u16(),
        if mode.bad_state { 400 } else { 200 }
    );
    let result = peer.recv().await?;
    let (raw, result) = match result.body {
        Body::Request { id, method, params } => {
            assert_eq!(id, 2);
            assert_eq!(method, "host.credential.replace");
            let raw = params["credential"]
                .as_str()
                .ok_or("credential replacement")?
                .to_owned();
            peer.response(id, Value::Null).await?;
            (Some(raw), peer.recv().await?)
        }
        _ => (None, result),
    };
    let value = match result.body {
        Body::Response { id: 2, result } => result,
        Body::Error { id: Some(2), error } => serde_json::to_value(error)?,
        _ => return Err("correlated auth result missing".into()),
    };
    Ok((server, peer, raw, value))
}

#[tokio::test]
async fn full_positive_login_local_signed_oidc_atomic_commit_status_logout() -> TestResult {
    let (server, mut peer, raw, result) = login_case(LoginMode::default()).await?;
    assert_eq!(result["authenticated"], true);
    assert_eq!(result["registration"], serde_json::to_value(registered())?);
    let raw = raw.ok_or("stored credential")?;
    let record =
        Credential::load(Some(&raw), &Options::parse(fixture::options())?)?.ok_or("record")?;
    assert_eq!(record.access_token, "local-access");
    assert!(record.usable(now()?));
    let recorded = server
        .requests
        .lock()
        .map_err(|_| "fixture requests")?
        .clone();
    let form = recorded
        .iter()
        .find(|request| request.path == "/token")
        .ok_or("token request")?
        .form();
    assert_eq!(form.get("code").map(String::as_str), Some("local-code"));
    assert!(form.get("redirect_uri").is_some_and(
        |value| value.starts_with("http://127.0.0.1:") && value.ends_with("/auth/callback")
    ));
    let requests_before = recorded.len();
    peer.request(3,"provider.auth",json!({"attempt_id":AuthAttemptId::new(),"operation":"status","options":fixture::options(),"credential":raw})).await?;
    assert!(
        matches!(peer.recv().await?.body, Body::Response { result,.. } if result["authenticated"] == true)
    );
    assert_eq!(
        server
            .requests
            .lock()
            .map_err(|_| "fixture requests")?
            .len(),
        requests_before
    );
    peer.request(4,"provider.auth",json!({"attempt_id":AuthAttemptId::new(),"operation":"logout","options":fixture::options(),"credential":record.serialize()?})).await?;
    let Body::Request { id, method, params } = peer.recv().await?.body else {
        return Err("logout replacement".into());
    };
    assert_eq!(method, "host.credential.replace");
    assert!(params["credential"].is_null());
    peer.response(id, Value::Null).await?;
    assert!(
        matches!(peer.recv().await?.body, Body::Response { result,.. } if result["authenticated"] == false && result["revocation_confirmed"] == true)
    );
    let recorded = server.requests.lock().map_err(|_| "requests")?;
    let form = recorded
        .iter()
        .find(|request| request.path == "/revoke")
        .ok_or("revoke request")?
        .form();
    assert_eq!(form.get("token").map(String::as_str), Some("local-refresh"));
    assert_eq!(
        form.get("token_type_hint").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some("oaiapp_local")
    );
    Ok(())
}

#[tokio::test]
async fn failed_state_never_exchanges_or_replaces_and_failed_identity_scope_never_commits()
-> TestResult {
    let (server, _peer, raw, result) = login_case(LoginMode {
        bad_state: true,
        ..LoginMode::default()
    })
    .await?;
    assert!(raw.is_none());
    assert_eq!(result["code"], "auth_failed");
    assert!(
        !server
            .requests
            .lock()
            .map_err(|_| "requests")?
            .iter()
            .any(|request| request.path == "/token")
    );
    for mode in [
        LoginMode {
            claims: json!({"nonce":"bad-nonce"}),
            ..LoginMode::default()
        },
        LoginMode {
            claims: json!({"exp":1}),
            ..LoginMode::default()
        },
        LoginMode {
            returning: true,
            existing: true,
            claims: json!({"sub":"different-account"}),
            ..LoginMode::default()
        },
        LoginMode {
            bad_signature: true,
            ..LoginMode::default()
        },
        LoginMode {
            claims: json!({"at_hash":"wrong-access-hash"}),
            ..LoginMode::default()
        },
        LoginMode {
            scope: Some("openid email profile offline_access resource.invoke".into()),
            ..LoginMode::default()
        },
    ] {
        let (_server, _peer, raw, result) = login_case(mode).await?;
        assert!(raw.is_none());
        assert!(result["code"] == "auth_failed" || result["code"] == "auth_scope_required");
        let encoded = result.to_string();
        assert!(
            !encoded.contains("bad-nonce")
                && !encoded.contains("local-access")
                && !encoded.contains("different-account")
        );
    }
    let (_server, _peer, raw, result) = login_case(LoginMode {
        returning: true,
        ..LoginMode::default()
    })
    .await?;
    assert!(raw.is_some());
    assert_eq!(result["authenticated"], true);
    Ok(())
}

#[tokio::test]
async fn refresh_rotates_tokens_and_validates_returned_identity_or_omission() -> TestResult {
    for (omit_id, omit_nonce) in [(false, false), (true, false), (false, true)] {
        let time = now()?;
        let previous = saved(time)?;
        let keys = fixture::jwks()?;
        let mut claims = fixture::claims("old-nonce", time);
        if omit_nonce {
            claims.as_object_mut().ok_or("claims")?.remove("nonce");
        }
        let replacement_id = fixture::signed(&claims)?;
        let server = Server::start(move |request,origin| match request.path.as_str() {
            "/discovery" => Reply::json(fixture::discovery(origin)),
            "/jwks" => Reply::json(keys.clone()),
            "/token" => {
                assert_eq!(request.method,"POST");
                let form = request.form();
                assert_eq!(form.get("grant_type").map(String::as_str),Some("refresh_token"));
                assert_eq!(form.get("refresh_token").map(String::as_str),Some("old-refresh"));
                assert_eq!(form.get("client_id").map(String::as_str),Some("oaiapp_local"));
                assert_eq!(form.get("resource").map(String::as_str),Some(RESOURCE)); assert!(!form.contains_key("scope"));
                let mut reply = json!({"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":3600});
                if !omit_id { reply["id_token"] = json!(replacement_id); reply["scope"] = json!(SCOPES); }
                Reply::json(reply)
            }
            _ => Reply::error(404,"fixture-not-found"),
        }).await?;
        let replacement = server.oauth()?.refresh(&previous).await?;
        assert_eq!(replacement.access_token, "new-access");
        assert_eq!(replacement.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(replacement.registration, previous.registration);
        assert!(replacement.usable(time));
        if omit_id {
            assert_eq!(replacement.id_token, previous.id_token);
        }
    }
    Ok(())
}

#[tokio::test]
async fn terminal_refresh_errors_security_failure_and_revoke_unavailable() -> TestResult {
    for code in [
        "invalid_grant",
        "invalid_refresh_token",
        "refresh_token_expired",
        "refresh_token_reused",
        "invalid_client",
    ] {
        let code = code.to_owned();
        let expected_client = code == "invalid_client";
        let server = Server::start(move |request, origin| match request.path.as_str() {
            "/discovery" => Reply::json(fixture::discovery(origin)),
            "/token" => Reply::error(400, &code),
            "/revoke" => Reply::error(503, "unavailable"),
            _ => Reply::error(404, "fixture-not-found"),
        })
        .await?;
        let previous = saved(now()?)?;
        let result = server.oauth()?.refresh(&previous).await;
        assert!(if expected_client {
            matches!(result, Err(Error::Registration))
        } else {
            matches!(result, Err(Error::AuthRequired))
        });
        assert!(!server.oauth()?.revoke(&previous).await);
        assert_eq!(
            server
                .requests
                .lock()
                .map_err(|_| "requests")?
                .iter()
                .filter(|request| request.path == "/token")
                .count(),
            1
        );
    }
    let previous = saved(now()?)?;
    let keys = fixture::jwks()?;
    let mut wrong = fixture::claims("refresh", now()?);
    wrong["sub"] = json!("another-account");
    let id = fixture::signed(&wrong)?;
    let server = Server::start(move |request,origin| match request.path.as_str() {
        "/discovery" => Reply::json(fixture::discovery(origin)), "/jwks" => Reply::json(keys.clone()),
        "/token" => Reply::json(json!({"access_token":"new","refresh_token":"new-refresh","id_token":id,"token_type":"Bearer","expires_in":3600,"scope":SCOPES})),
        _ => Reply::error(404,"fixture"),
    }).await?;
    assert!(matches!(
        server.oauth()?.refresh(&previous).await,
        Err(Error::Identity)
    ));
    Ok(())
}

#[test]
fn credentials_scope_expiry_rotation_and_registration_validation() -> TestResult {
    let time = 2_000_000_000;
    let record = saved(time)?;
    let options = Options::parse(fixture::options())?;
    assert!(Credential::load(Some(&record.serialize()?), &options)?.is_some());
    assert!(record.usable(time + 3601));
    assert!(!record.usable(time + REFRESH_LIFETIME + 1));
    assert!(OAuth::needs_refresh(&record, time + 3541));
    for mutation in [
        json!({"access_token":"contains\nnewline"}),
        json!({"version":9}),
        json!({"registration":{"version":1,"client_id":"dynamic_agent_client","subject":"local-account","issuer":ISSUER,"host_id":options.host_id}}),
    ] {
        let mut raw = serde_json::to_value(&record)?;
        for (key, value) in mutation.as_object().ok_or("mutation")? {
            raw[key] = value.clone();
        }
        assert!(Credential::load(Some(&raw.to_string()), &options).is_err());
    }
    let mut scope = serde_json::to_value(&record)?;
    scope["scopes"] = json!(["openid"]);
    let scope = Credential::load(Some(&scope.to_string()), &options)?.ok_or("scope record")?;
    assert!(!scope.usable(time));
    let token: TokenResponse = serde_json::from_value(
        json!({"access_token":"new","token_type":"Bearer","expires_in":3600}),
    )?;
    assert!(matches!(
        credential(token, registered(), time, Some(&record), None),
        Err(Error::Identity)
    ));
    Ok(())
}

#[tokio::test]
async fn logout_clears_even_unconfirmed_and_status_is_offline() -> TestResult {
    let previous = saved(now()?)?;
    let server = Server::start(|request, origin| match request.path.as_str() {
        "/discovery" => Reply::json(fixture::discovery(origin)),
        "/revoke" => Reply::error(503, "unavailable"),
        _ => Reply::error(404, "fixture"),
    })
    .await?;
    let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
    peer.initialize().await?;
    peer.request(2,"provider.auth",json!({"attempt_id":AuthAttemptId::new(),"operation":AuthOperation::Status,"options":fixture::options(),"credential":null})).await?;
    assert!(
        matches!(peer.recv().await?.body, Body::Response { result,.. } if result["authenticated"] == false)
    );
    assert!(server.requests.lock().map_err(|_| "requests")?.is_empty());
    for (id, raw) in [
        (3, previous.serialize()?),
        (4, "malformed-credential".into()),
    ] {
        peer.request(id,"provider.auth",json!({"attempt_id":AuthAttemptId::new(),"operation":"logout","options":fixture::options(),"credential":raw})).await?;
        let Body::Request { id, method, params } = peer.recv().await?.body else {
            return Err("clear request".into());
        };
        assert_eq!(method, "host.credential.replace");
        assert!(params["credential"].is_null());
        peer.response(id, Value::Null).await?;
        assert!(
            matches!(peer.recv().await?.body, Body::Response { result,.. } if result["authenticated"] == false && result["revocation_confirmed"] == false)
        );
    }
    Ok(())
}

#[tokio::test]
async fn discovery_cannot_redirect_token_or_key_material_to_another_authority() -> TestResult {
    let server = Server::start(|_request, origin| {
        let mut doc = fixture::discovery(origin);
        doc["jwks_uri"] = json!("https://attacker.invalid/jwks");
        Reply::json(doc)
    })
    .await?;
    assert!(matches!(
        server.oauth()?.discovery().await,
        Err(Error::Identity)
    ));
    assert_eq!(server.requests.lock().map_err(|_| "requests")?.len(), 1);
    Ok(())
}
