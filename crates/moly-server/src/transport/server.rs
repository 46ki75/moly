//! Server-side protocol adapter; only Core commands/effects cross this module.
use super::{Incoming, Peer, local};
use crate::core::{Connection, Output, Server};
use moly_protocol::{Initialized, ProtocolError, SERVER_VERSION};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    task::JoinSet,
};

struct CloseOnDrop(Connection);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}
/// Serve one connection. Dropping it never cancels its sessions or runs.
pub async fn serve_connection<S>(server: Server, stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (peer, mut inbox) = Peer::spawn(stream);
    let (connection, mut output) = Connection::new();
    let _guard = CloseOnDrop(connection.clone());
    tracing::debug!(server_id = %server.id(), connection_id = %connection.id(), "connection attached");
    let mut initialized = false;
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = connection.closed() => break,
            _ = peer.closed() => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => { if result.is_err() { break; } },
            effect = output.recv() => match effect {
                None => break,
                Some(Output::Event(event)) => {
                    let Ok(params) = serde_json::to_value(event) else { break; };
                    // Runtime backpressure can close this connection while its
                    // writer is already full. Do not trap closure behind the send.
                    tokio::select! {
                        biased;
                        _ = connection.closed() => break,
                        result = peer.event("session.event", params) => { if result.is_err() { break; } },
                    }
                }
                Some(Output::Tool { request, reply }) => {
                    let peer = peer.clone();
                    tasks.spawn(async move {
                        let result = async {
                            let params = serde_json::to_value(request).map_err(|_| ProtocolError::new("internal", "invalid tool request"))?;
                            let value = peer.request("tool.execute", params).await.map_err(|_| ProtocolError::new("executor_lost", "executor request failed"))?;
                            serde_json::from_value(value).map_err(|_| ProtocolError::new("invalid_tool_result", "invalid executor result"))
                        };
                        let result = tokio::time::timeout(std::time::Duration::from_secs(60), result).await
                            .unwrap_or_else(|_| Err(ProtocolError::new("tool_timeout", "executor did not respond")));
                        let _ = reply.send(result);
                    });
                }
            },
            incoming = inbox.recv() => match incoming {
                None => break,
                Some(Incoming::Event { .. }) => {},
                Some(Incoming::Request { id, method, params }) => {
                    if method == "initialize" {
                        let result = if initialized {
                            Err(ProtocolError::new("already_initialized", "connection already initialized"))
                        } else if params.get("protocol_version").and_then(Value::as_u64) != Some(u64::from(SERVER_VERSION)) {
                            Err(ProtocolError::new("incompatible_version", "unsupported protocol version"))
                        } else {
                            initialized = true;
                            serde_json::to_value(Initialized { server_id: server.id(), role: "server".into(), protocol_version: SERVER_VERSION })
                                .map_err(|_| ProtocolError::new("internal", "cannot encode handshake"))
                        };
                        if peer.respond(id, result).await.is_err() { break; }
                    } else if !initialized {
                        if peer.respond(id, Err(ProtocolError::new("not_initialized", "initialize first"))).await.is_err() { break; }
                    } else {
                        if tasks.len() >= 128 { break; }
                        let server = server.clone();
                        let connection = connection.clone();
                        let peer = peer.clone();
                        tasks.spawn(async move {
                            let result = server.request(&connection, &method, params).await;
                            if peer.respond(id, result).await.is_err() { peer.close(); }
                        });
                    }
                }
            }
        }
    }
    peer.close();
    tasks.abort_all();
    server.disconnect(&connection).await;
    tracing::debug!(server_id = %server.id(), connection_id = %connection.id(), "connection detached");
}
/// Accept multiple direct Clients on one logical endpoint until shutdown.
pub async fn serve(
    server: Server,
    listener: local::Listener,
    shutdown: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            stream = listener.accept() => { connections.spawn(serve_connection(server.clone(), stream?)); }
        }
    }
    server.shutdown();
    connections.abort_all();
    Ok(())
}
