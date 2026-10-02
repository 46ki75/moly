use crate::core::{Connection, Server};
use crate::tests::provider_process;
use crate::transport::{encode_frame, read_frame, serve_connection};
use moly_protocol::{Body, Message, SERVER_VERSION};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};

type Error = Box<dyn std::error::Error + Send + Sync>;
async fn rpc(
    raw: &mut BufReader<DuplexStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, Error> {
    raw.write_all(&encode_frame(&Message::new(Body::Request {
        id,
        method: method.into(),
        params,
    }))?)
    .await?;
    loop {
        match read_frame(raw).await?.ok_or("unexpected EOF")?.body {
            Body::Response { id: reply, result } if reply == id => return Ok(result),
            Body::Error { error, .. } => return Err(error.into()),
            _ => {}
        }
    }
}
#[tokio::test]
async fn slow_consumer_closure_interrupts_a_blocked_event_enqueue() -> Result<(), Error> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let server = Server::new()?;
        let (admin, _effects) = Connection::new();
        // Owned, never-responding local endpoint; no external provider/service.
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        server.request(&admin, "config.apply", json!({"base_revision":0,"config":{
            "provider": provider_process::provider(json!({"model_endpoint":format!("http://{}/chat", http.local_addr()?), "model":"test"}))?,
            "workspace":std::env::temp_dir(),"secret_ref":null
        }})).await?;
        let session = server.request(&admin, "session.create", Value::Null).await?["session_id"].clone();
        let (stream, remote) = tokio::io::duplex(1024);
        let mut serving = tokio::spawn(serve_connection(server.clone(), stream));
        let mut raw = BufReader::new(remote);
        rpc(&mut raw, 1, "initialize", json!({"protocol_version":SERVER_VERSION})).await?;
        let registration = json!({"session_id":session,"tools":[{"name":"probe","description":"probe","input_schema":{}}]});
        rpc(&mut raw, 2, "tools.register", registration.clone()).await?;
        rpc(&mut raw, 3, "subscribe", json!({"session_id":session,"after_seq":0})).await?;
        // Never read again. Fill transport, wire writer, and connection event queues.
        for _ in 0..450 {
            let started = server.request(&admin, "run.start", json!({"session_id":session,"message":"message"})).await?;
            let _ = server.request(&admin, "run.cancel", json!({"session_id":session,"run_id":started["run_id"]})).await;
        }
        // The name is freed only when the original connection has been closed.
        server.request(&admin, "tools.register", registration).await?;
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), &mut serving).await;
        if stopped.is_err() { serving.abort(); }
        stopped.expect("a closed slow consumer must interrupt an already-blocked event send")?;
        drop(raw);
        let too_old = server.request(&admin, "subscribe", json!({"session_id":session,"after_seq":0})).await;
        assert_eq!(too_old.expect_err("bounded history must reject expired cursors").code, "replay_unavailable");
        server.shutdown();
        Ok::<_, Error>(())
    }).await?
}
