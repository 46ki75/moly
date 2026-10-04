//! Hermetic loopback fixtures. The embedded RSA private key is synthetic test data.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use moly_protocol::{Body, Message};
use reqwest::Url;
use rsa::pkcs8::DecodePrivateKey;
use rsa::traits::PublicKeyParts;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config::ISSUER;
use crate::oauth::{Endpoints, OAuth};
use crate::service::Service;
use crate::transport::{encode_frame, read_frame};

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
const PRIVATE: &[u8] = include_bytes!("../tests/fixtures/oidc-test-key.pem");

pub(crate) fn signed(claims: &Value) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("local-key".into());
    Ok(jsonwebtoken::encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(PRIVATE)?,
    )?)
}

pub(crate) fn jwks() -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(std::str::from_utf8(PRIVATE)?)?;
    Ok(
        json!({"keys":[{"kty":"RSA", "kid":"local-key", "use":"sig", "alg":"RS256", "key_ops":["verify"], "n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()), "e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]}),
    )
}

pub(crate) fn claims(nonce: &str, now: u64) -> Value {
    json!({"iss":ISSUER, "aud":"oaiapp_local", "sub":"local-account", "exp":now+3600, "iat":now, "nonce":nonce})
}

#[derive(Clone)]
pub(crate) struct Request {
    pub(crate) path: String,
    pub(crate) method: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: Vec<u8>,
}
impl Request {
    pub(crate) fn form(&self) -> BTreeMap<String, String> {
        // Only decoding, never a network request.
        let url = Url::parse("http://127.0.0.1/").expect("fixed test URL");
        let mut url = url;
        url.set_query(std::str::from_utf8(&self.body).ok());
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    pub(crate) content_type: &'static str,
}
impl Reply {
    pub(crate) fn json(value: Value) -> Self {
        Self {
            status: 200,
            body: value.to_string().into_bytes(),
            content_type: "application/json",
        }
    }
    pub(crate) fn error(status: u16, code: &str) -> Self {
        Self {
            status,
            ..Self::json(json!({"error":code}))
        }
    }
}

pub(crate) struct Server {
    pub(crate) origin: String,
    pub(crate) requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub(crate) async fn start(
        handler: impl Fn(Request, &str) -> Reply + Send + Sync + 'static,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let base = origin.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(value) => value,
                    Err(_) => return,
                };
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let count = stream
                        .read(&mut buffer)
                        .await
                        .expect("read local HTTP request");
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    assert!(bytes.len() <= 2 * 1024 * 1024);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = std::str::from_utf8(&bytes[..header_end])
                    .expect("HTTP headers are UTF-8")
                    .to_owned();
                let mut lines = header.split("\r\n");
                let mut line = lines.next().expect("request line").split_whitespace();
                let method = line.next().expect("method").to_owned();
                let path = line.next().expect("path").to_owned();
                let headers: BTreeMap<_, _> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                let length: usize = headers
                    .get("content-length")
                    .map(|value| value.parse().expect("content length"))
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let count = stream.read(&mut buffer).await.expect("read body");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let request = Request {
                    path,
                    method,
                    headers,
                    body: bytes[header_end..header_end + length].to_vec(),
                };
                recorded
                    .lock()
                    .expect("fixture mutex")
                    .push(request.clone());
                let response = handler(request, &base);
                let header = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.status,
                    response.content_type,
                    response.body.len()
                );
                if stream.write_all(header.as_bytes()).await.is_ok() {
                    let _ = stream.write_all(&response.body).await;
                }
            }
        });
        Ok(Self {
            origin,
            requests,
            task,
        })
    }

    pub(crate) fn oauth(&self) -> Result<OAuth, crate::error::Error> {
        Ok(OAuth {
            client: crate::http::client()?,
            endpoints: Endpoints {
                discovery: format!("{}/discovery", self.origin),
                responses: format!("{}/v1/responses", self.origin),
                test_origin: Some(self.origin.clone()),
            },
        })
    }
}

pub(crate) fn discovery(origin: &str) -> Value {
    json!({"issuer":ISSUER, "authorization_endpoint":format!("{ISSUER}/api/accounts/authorize"), "token_endpoint":format!("{origin}/token"), "jwks_uri":format!("{origin}/jwks"), "revocation_endpoint":format!("{origin}/revoke"), "id_token_signing_alg_values_supported":["RS256"]})
}

pub(crate) struct Peer {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    pub(crate) task: JoinHandle<Result<(), crate::transport::TransportError>>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Peer {
    pub(crate) fn start(service: Service) -> Self {
        let (peer, provider) = tokio::io::duplex(2 * 1024 * 1024);
        let (reader, writer) = tokio::io::split(provider);
        let task = tokio::spawn(crate::transport::serve(
            BufReader::new(reader),
            writer,
            service,
        ));
        let (reader, writer) = tokio::io::split(peer);
        Self {
            reader: BufReader::new(reader),
            writer,
            task,
        }
    }
    pub(crate) async fn send(&mut self, message: Message) -> TestResult {
        self.writer.write_all(&encode_frame(&message)?).await?;
        Ok(())
    }
    pub(crate) async fn request(&mut self, id: u64, method: &str, params: Value) -> TestResult {
        self.send(Message::new(Body::Request {
            id,
            method: method.into(),
            params,
        }))
        .await
    }
    pub(crate) async fn response(&mut self, id: u64, result: Value) -> TestResult {
        self.send(Message::new(Body::Response { id, result })).await
    }
    pub(crate) async fn recv(
        &mut self,
    ) -> Result<Message, Box<dyn std::error::Error + Send + Sync>> {
        Ok(tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_frame(&mut self.reader),
        )
        .await??
        .ok_or("unexpected EOF")?)
    }
    pub(crate) async fn initialize(&mut self) -> TestResult {
        self.request(1, "initialize", json!({"protocol_version":2}))
            .await?;
        assert!(
            matches!(self.recv().await?.body, Body::Response { id:1, result } if result["protocol_version"] == 2)
        );
        Ok(())
    }
    pub(crate) async fn eof(&mut self) -> TestResult {
        self.writer.shutdown().await?;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut self.task)
                .await??
                .is_ok()
        );
        Ok(())
    }
}

pub(crate) fn options() -> Value {
    json!({"model":"explicit-model", "host_id":"urn:uuid:00000000-0000-4000-8000-000000000001"})
}
