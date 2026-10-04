use std::time::Duration;

use moly_protocol::{AuthAttemptId, Body, MAX_FRAME_BYTES, Message};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::*;
use crate::oauth::{Endpoints, OAuth};
use crate::service::Service;
use crate::test_support::{self as fixture, Peer, Reply, Server, TestResult};

#[tokio::test]
async fn headless_interaction_error_is_not_a_protocol_fault() -> TestResult {
    let server = Server::start(|_, origin| Reply::json(fixture::discovery(origin))).await?;
    let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
    peer.initialize().await?;
    peer.request(2, "provider.auth", json!({"attempt_id":AuthAttemptId::new(),"operation":"login","options":fixture::options(),"credential":null})).await?;
    let Body::Request { id, method, .. } = peer.recv().await?.body else {
        return Err("interaction expected".into());
    };
    assert_eq!(method, "host.interact");
    peer.send(Message::new(Body::Error {
        id: Some(id),
        error: moly_protocol::ProtocolError::new(
            "interaction_unavailable",
            "private-host-diagnostic",
        ),
    }))
    .await?;
    let reply = peer.recv().await?;
    assert!(!serde_json::to_string(&reply)?.contains("private-host-diagnostic"));
    assert!(
        matches!(reply.body, Body::Error { error, .. } if error.code == "auth_interaction_unavailable")
    );
    peer.eof().await?;
    Ok(())
}

#[tokio::test]
async fn framing_is_bounded_and_rejects_partial_utf8_and_envelopes() -> TestResult {
    for bytes in [
        b"[]\n".to_vec(),
        b"{}\n".to_vec(),
        b"{bad}\n".to_vec(),
        vec![0xff, b'\n'],
        b"{\"version\":99,\"type\":\"request\",\"id\":1,\"method\":\"x\"}\n".to_vec(),
        b"{\"version\":1,\"type\":\"future\"}\n".to_vec(),
        b"{}".to_vec(),
        vec![b'x'; MAX_FRAME_BYTES + 1],
    ] {
        assert!(
            read_frame(&mut BufReader::new(bytes.as_slice()))
                .await
                .is_err()
        );
    }
    let request = Message::new(Body::Request {
        id: 1,
        method: "initialize".into(),
        params: json!({"protocol_version":2}),
    });
    let bytes = encode_frame(&request)?;
    let mut combined = bytes.clone();
    combined.extend_from_slice(&bytes);
    let mut reader = BufReader::with_capacity(1, combined.as_slice());
    assert!(read_frame(&mut reader).await?.is_some());
    assert!(read_frame(&mut reader).await?.is_some());
    assert!(read_frame(&mut reader).await?.is_none());
    let empty = Message::new(Body::Response {
        id: 1,
        result: json!(""),
    });
    let overhead = encode_frame(&empty)?.len() - 1;
    let exact = Message::new(Body::Response {
        id: 1,
        result: json!("x".repeat(MAX_FRAME_BYTES - overhead)),
    });
    assert_eq!(encode_frame(&exact)?.len(), MAX_FRAME_BYTES + 1);
    assert!(
        read_frame(&mut BufReader::new(encode_frame(&exact)?.as_slice()))
            .await?
            .is_some()
    );
    let oversized = Message::new(Body::Response {
        id: 1,
        result: json!("x".repeat(MAX_FRAME_BYTES - overhead + 1)),
    });
    assert!(encode_frame(&oversized).is_err());
    Ok(())
}

#[tokio::test]
async fn handshake_validation_unknown_methods_and_local_auth_required() -> TestResult {
    let mut peer = Peer::start(Service::new()?);
    peer.request(1, "provider.validate", fixture::options())
        .await?;
    assert!(
        matches!(peer.recv().await?.body,Body::Error { error,.. } if error.code == "not_initialized")
    );
    peer.request(2, "initialize", json!({"protocol_version":1}))
        .await?;
    assert!(
        matches!(peer.recv().await?.body,Body::Error { error,.. } if error.code == "incompatible_version")
    );
    peer.request(3, "initialize", json!({"protocol_version":2,"future":true}))
        .await?;
    assert!(
        matches!(peer.recv().await?.body,Body::Response { result,.. } if result["role"] == "model_provider")
    );
    peer.request(4, "initialize", json!({"protocol_version":2}))
        .await?;
    assert!(
        matches!(peer.recv().await?.body,Body::Error { error,.. } if error.code == "already_initialized")
    );
    peer.request(5, "future.method", json!({"credential":"secret-sentinel"}))
        .await?;
    let reply = peer.recv().await?;
    assert!(!serde_json::to_string(&reply)?.contains("secret-sentinel"));
    assert!(matches!(reply.body,Body::Error { error,.. } if error.code == "unknown_method"));
    peer.request(6, "provider.validate", fixture::options())
        .await?;
    assert!(matches!(
        peer.recv().await?.body,
        Body::Response {
            result: Value::Null,
            ..
        }
    ));
    peer.request(7, "provider.step", model_request(None))
        .await?;
    assert!(
        matches!(peer.recv().await?.body,Body::Error { error,.. } if error.code == "auth_required")
    );
    peer.eof().await?;
    Ok(())
}

#[tokio::test]
async fn zero_reused_requests_and_uncorrelated_replies_fail_closed() -> TestResult {
    for message in [
        Message::new(Body::Request {
            id: 0,
            method: "initialize".into(),
            params: json!({"protocol_version":2}),
        }),
        Message::new(Body::Response {
            id: 1,
            result: Value::Null,
        }),
        Message::new(Body::Error {
            id: None,
            error: Error::Host.protocol(),
        }),
    ] {
        let (mut peer, provider) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(provider);
        let service = Service::new()?;
        let task = tokio::spawn(serve(BufReader::new(reader), writer, service));
        peer.write_all(&encode_frame(&message)?).await?;
        assert!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await??
                .is_err()
        );
    }
    let mut peer = Peer::start(Service::new()?);
    peer.initialize().await?;
    peer.request(1, "provider.validate", fixture::options())
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), &mut peer.task)
            .await??
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn wrong_host_id_or_malformed_replace_ack_fail_without_accepting_peer_diagnostics()
-> TestResult {
    let server = Server::start(|_, _| Reply::error(404, "unused")).await?;
    for wrong_id in [true, false] {
        let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
        peer.initialize().await?;
        peer.request(2,"provider.auth",json!({"attempt_id":AuthAttemptId::new(),"operation":"logout","options":fixture::options(),"credential":null})).await?;
        let Body::Request { id, method, .. } = peer.recv().await?.body else {
            return Err("reverse request".into());
        };
        assert_eq!(method, "host.credential.replace");
        if wrong_id {
            peer.response(id + 1, Value::Null).await?;
            assert!(
                tokio::time::timeout(Duration::from_secs(2), &mut peer.task)
                    .await??
                    .is_err()
            );
        } else {
            peer.response(id, json!({"credential":"private-sentinel"}))
                .await?;
            let reply = peer.recv().await?;
            assert!(!serde_json::to_string(&reply)?.contains("private-sentinel"));
            assert!(
                matches!(reply.body,Body::Error { error,.. } if error.code == "provider_protocol")
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn eof_cancels_waiting_loopback_callback_and_http_work() -> TestResult {
    let server = Server::start(|_, origin| Reply::json(fixture::discovery(origin))).await?;
    let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
    peer.initialize().await?;
    let attempt = AuthAttemptId::new();
    peer.request(2,"provider.auth",json!({"attempt_id":attempt,"operation":"login","options":fixture::options(),"credential":null})).await?;
    let Body::Request { id, params, .. } = peer.recv().await?.body else {
        return Err("interaction".into());
    };
    let url = reqwest::Url::parse(params["url"].as_str().ok_or("URL")?)?;
    let redirect = url
        .query_pairs()
        .find(|(name, _)| name == "redirect_uri")
        .ok_or("redirect")?
        .1
        .into_owned();
    peer.response(id, json!({"attempt_id":attempt,"outcome":"opened"}))
        .await?;
    peer.eof().await?;
    assert!(crate::http::client()?.get(redirect).send().await.is_err());

    // An HTTP endpoint that never returns headers. EOF must finish well before
    // the HTTP deadline and close the underlying connection, not merely detach it.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let (started, received) = oneshot::channel();
    let remote = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).await?;
        assert!(count > 0);
        let _ = started.send(());
        loop {
            if stream.read(&mut buffer).await? == 0 {
                break;
            }
        }
        Ok::<_, std::io::Error>(())
    });
    let oauth = OAuth {
        client: crate::http::client()?,
        endpoints: Endpoints {
            discovery: format!("{origin}/discovery"),
            responses: format!("{origin}/responses"),
            test_origin: Some(origin),
        },
    };
    let mut peer = Peer::start(Service::with_oauth(oauth));
    peer.initialize().await?;
    peer.request(2,"provider.auth",json!({"attempt_id":AuthAttemptId::new(),"operation":"login","options":fixture::options(),"credential":null})).await?;
    tokio::time::timeout(Duration::from_secs(2), received).await??;
    peer.eof().await?;
    tokio::time::timeout(Duration::from_secs(2), remote).await???;
    Ok(())
}

fn model_request(credential: Option<String>) -> Value {
    json!({"options":fixture::options(),"credential":credential,"context":{"session_id":moly_protocol::SessionId::new(),"run_id":moly_protocol::RunId::new(),"model_call_id":moly_protocol::ModelCallId::new(),"call_kind":"primary"},"messages":[{"kind":"user","text":"fixture question"}],"tools":[]})
}

fn expired_record() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let time = crate::oauth::now()?;
    Ok(json!({"version":1,"registration":{"version":1,"issuer":crate::config::ISSUER,"subject":"local-account","client_id":"oaiapp_local","host_id":fixture::options()["host_id"]},"access_token":"old-access","refresh_token":"old-refresh","id_token":fixture::signed(&fixture::claims("old",time-120))?,"oidc":{"nonce":"old","audience":"oaiapp_local","auth_time":null},"scopes":crate::config::SCOPES.split_ascii_whitespace().collect::<Vec<_>>(),"expires_at":time-1,"refresh_expires_at":time+3600,"saved_at":time-120,"earliest_refresh_at":null}).to_string())
}

#[tokio::test]
async fn earliest_refresh_does_not_allow_expired_inference_or_erase_renewable_credentials()
-> TestResult {
    let server = Server::start(|_, _| Reply::error(500, "unexpected-network")).await?;
    let mut record: Value = serde_json::from_str(&expired_record()?)?;
    record["earliest_refresh_at"] = json!(crate::oauth::now()? + 600);
    let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
    peer.initialize().await?;
    peer.request(2, "provider.step", model_request(Some(record.to_string())))
        .await?;
    assert!(
        matches!(peer.recv().await?.body, Body::Error { error, .. } if error.code == "auth_required")
    );
    assert!(server.requests.lock().map_err(|_| "requests")?.is_empty());
    Ok(())
}

#[tokio::test]
async fn model_refresh_commit_precedes_inference_and_invalid_grant_clears() -> TestResult {
    for invalid_grant in [false, true] {
        let keys = fixture::jwks()?;
        let identity = fixture::signed(&fixture::claims("old", crate::oauth::now()?))?;
        let server = Server::start(move |request,origin| match request.path.as_str() {
            "/discovery" => Reply::json(fixture::discovery(origin)),
            "/jwks" => Reply::json(keys.clone()),
            "/token" if invalid_grant => Reply::error(400,"invalid_grant"),
            "/token" => Reply::json(json!({"access_token":"new-access","refresh_token":"new-refresh","id_token":identity,"token_type":"Bearer","expires_in":3600,"scope":crate::config::SCOPES})),
            "/v1/responses" => {
                assert_eq!(request.headers.get("authorization").map(String::as_str),Some("Bearer new-access"));
                let value: Value = serde_json::from_slice(&request.body).expect("Responses request");
                assert_eq!(value["store"],false); assert_eq!(value["stream"],true);
                let event = json!({"type":"response.completed","response":{"id":"fixture-response","status":"completed","output":[{"type":"message","id":"msg","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"local complete","annotations":[]}]}]}});
                Reply { status:200,body:format!("event: response.completed\ndata: {event}\n\n").into_bytes(),content_type:"text/event-stream" }
            }
            _ => Reply::error(404,"fixture"),
        }).await?;
        let mut peer = Peer::start(Service::with_oauth(server.oauth()?));
        peer.initialize().await?;
        peer.request(2, "provider.step", model_request(Some(expired_record()?)))
            .await?;
        let Body::Request { id, method, params } = peer.recv().await?.body else {
            return Err("rotation commit".into());
        };
        assert_eq!(method, "host.credential.replace");
        if invalid_grant {
            assert!(params["credential"].is_null());
        } else {
            let raw = params["credential"].as_str().ok_or("new record")?;
            assert_eq!(
                serde_json::from_str::<Value>(raw)?["refresh_token"],
                "new-refresh"
            );
        }
        assert!(
            !server
                .requests
                .lock()
                .map_err(|_| "requests")?
                .iter()
                .any(|request| request.path == "/v1/responses")
        );
        peer.response(id, Value::Null).await?;
        let result = peer.recv().await?;
        if invalid_grant {
            assert!(
                matches!(result.body,Body::Error { error,.. } if error.code == "auth_required")
            );
        } else {
            assert!(
                matches!(result.body,Body::Response { result,.. } if result["outcome"] == "completed" && result["text"] == "local complete")
            );
        }
        let requests = server.requests.lock().map_err(|_| "requests")?;
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path == "/token")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path == "/v1/responses")
                .count(),
            usize::from(!invalid_grant)
        );
    }
    Ok(())
}
