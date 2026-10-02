//! Semantic Client implementation behind the SDK's public facade.
use crate::transport::{self, Incoming, Peer};
use moly_protocol::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};
use tokio::{sync::mpsc, task::JoinSet};

/// Recoverable connection, encoding, or Server-operation failure.
///
/// Match [`Error::Remote`] by its machine-readable code, not its message.
/// Connection loss or cancellation of a request future leaves its outcome uncertain;
/// inspect/replay before retrying side effects. Transport internals are not public API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Opening or using the byte stream failed.
    #[error("stream I/O failed")]
    Io(#[from] std::io::Error),
    /// Invalid framing, encoding, version, or envelope; contains no payload.
    #[error("invalid protocol frame: {0}")]
    Frame(&'static str),
    /// Connection closed, including fail-closed backpressure handling.
    #[error("connection closed")]
    Closed,
    /// The bounded number of outstanding requests was exhausted.
    #[error("outstanding request limit reached")]
    Busy,
    /// The Server rejected an operation without necessarily closing the connection.
    #[error(transparent)]
    Remote(#[from] ProtocolError),
    /// Invalid operation parameters or response.
    #[error("unexpected protocol value")]
    Json(#[from] serde_json::Error),
    /// Endpoint was not a compatible Server.
    #[error("incompatible Server handshake")]
    Handshake,
}
#[doc(hidden)]
impl From<transport::Error> for Error {
    fn from(error: transport::Error) -> Self {
        match error {
            transport::Error::Io(error) => Self::Io(error),
            transport::Error::Frame(message) => Self::Frame(message),
            transport::Error::Closed => Self::Closed,
            transport::Error::Busy => Self::Busy,
            transport::Error::Remote(error) => Self::Remote(error),
        }
    }
}
type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, ProtocolError>> + Send>>;
type Handler = Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>;
#[derive(Default)]
struct ToolRegistry {
    handlers: HashMap<(SessionId, String), Handler>,
    closed: bool,
}
type Tools = Arc<RwLock<ToolRegistry>>;
struct Inner {
    peer: Peer,
    tools: Tools,
    initialized: Initialized,
    registration: tokio::sync::Mutex<()>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.peer.close();
    }
}
struct Connecting(Option<Peer>);
impl Drop for Connecting {
    fn drop(&mut self) {
        if let Some(peer) = &self.0 {
            peer.close();
        }
    }
}
struct Registering<'a>(Option<&'a Client>);
impl Drop for Registering<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.0 {
            // An interrupted registration has unknown Server authority; never keep
            // potentially mismatched handlers or their captured Client cycles alive.
            client.close();
        }
    }
}
/// Cloneable, concurrent Client sharing one full-duplex Server connection.
///
/// Uses the caller's Tokio runtime and never spawns a Server, discovers configuration,
/// initializes a provider, or installs a tracing subscriber. Dropping the last clone
/// closes only this connection; sessions and runs remain Server-owned. Call
/// [`Client::close`] explicitly to break callback ownership cycles.
#[derive(Clone)]
pub struct Client(Arc<Inner>);
/// Bounded canonical event stream for every session subscribed on this connection.
///
/// Keep and drain this receiver while subscribed. Overflow or delivery to a dropped
/// receiver closes the connection instead of silently losing events. Track each
/// session's last sequence and reconnect explicitly to request retained replay.
pub struct Events(mpsc::Receiver<SessionEvent>);
impl Events {
    /// Receive the next canonical event; `None` means the connection's dispatcher
    /// has stopped and buffered events have been drained. Canceling this receive
    /// future does not consume an event.
    pub async fn recv(&mut self) -> Option<SessionEvent> {
        self.0.recv().await
    }
}
/// One Client-owned executable capability. The callback does not own ToolRun state.
pub struct Tool {
    definition: ToolDefinition,
    handler: Handler,
}
impl Tool {
    /// Define an asynchronous hosted tool, automatically bound to the issued lease.
    ///
    /// Callbacks may make nested Client requests; they must not block the runtime.
    /// Connection loss aborts callback futures, not already-performed side effects.
    /// Prefer weak captures when referencing a Client from its own callback, or
    /// explicitly call [`Client::close`] to release the registration cycle.
    pub fn new<F, Fut>(definition: ToolDefinition, execute: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ProtocolError>> + Send + 'static,
    {
        Self {
            definition,
            handler: Arc::new(move |arguments| Box::pin(execute(arguments))),
        }
    }
}
impl Client {
    /// Connect to an existing local Server and verify its role and protocol version.
    ///
    /// Endpoints are filesystem paths on Unix and named-pipe names (without the
    /// `\\.\pipe\` prefix) on Windows. No Server is spawned and no config is resolved.
    /// Requires Tokio I/O and timers. Apply a caller-selected deadline with
    /// [`tokio::time::timeout`]; canceling this future closes any partial connection.
    pub async fn connect(endpoint: &str) -> Result<(Self, Events), Error> {
        let stream = transport::local::connect(endpoint)
            .await
            .map_err(transport::Error::from)?;
        Self::from_stream(stream).await
    }
    /// Attach using an owned, full-duplex Tokio byte stream with the same handshake.
    ///
    /// The caller supplies transport security. No reconnect, retries, or implicit
    /// deadline are installed. Canceling establishment closes the partial connection.
    pub async fn from_stream<S>(stream: S) -> Result<(Self, Events), Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (peer, mut inbox) = Peer::spawn(stream);
        // Cancellation of the handshake must also stop the detached dispatcher.
        let mut connecting = Connecting(Some(peer.clone()));
        let tools: Tools = Arc::default();
        let handlers = tools.clone();
        let dispatcher = peer.clone();
        let (events_tx, events_rx) = mpsc::channel(2048);
        tokio::spawn(async move {
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = dispatcher.closed() => break,
                    Some(_) = requests.join_next(), if !requests.is_empty() => {},
                    incoming = inbox.recv() => match incoming {
                        None => break,
                        Some(Incoming::Event { event, params }) if event == "session.event" => {
                            let Ok(event) = serde_json::from_value(params) else { break; };
                            if events_tx.try_send(event).is_err() { break; }
                        }
                        Some(Incoming::Event { .. }) => {},
                        Some(Incoming::Request { id, method, params }) => {
                            if requests.len() >= 128 { break; }
                            let peer = dispatcher.clone();
                            let handlers = handlers.clone();
                            requests.spawn(async move {
                                let result = execute(&handlers, &method, params).await;
                                let _ = peer.respond(id, result).await;
                            });
                        }
                    }
                }
            }
            dispatcher.close();
            // Dropping JoinSet aborts callbacks on connection loss, not external side effects.
        });
        let handshake = peer
            .request("initialize", json!({"protocol_version":SERVER_VERSION}))
            .await;
        let initialized: Initialized = match handshake.and_then(|value| {
            serde_json::from_value(value).map_err(|_| transport::Error::Frame("invalid handshake"))
        }) {
            Ok(value) => value,
            Err(error) => {
                peer.close();
                return Err(error.into());
            }
        };
        if initialized.role != "server" || initialized.protocol_version != SERVER_VERSION {
            peer.close();
            return Err(Error::Handshake);
        }
        connecting.0.take();
        Ok((
            Self(Arc::new(Inner {
                peer,
                tools,
                initialized,
                registration: tokio::sync::Mutex::new(()),
            })),
            Events(events_rx),
        ))
    }
    /// Connected authority, verified rather than inferred from endpoint metadata.
    pub fn server_id(&self) -> ServerId {
        self.0.initialized.server_id
    }
    /// Disconnect all clones and release callbacks without requesting run cancellation.
    ///
    /// A run relying on this connection's executor can fail with `executor_lost`.
    /// Call explicitly if a callback captures a clone of this Client (an Arc cycle).
    pub fn close(&self) {
        self.0.peer.close();
        let mut registry = self.0.tools.write().expect("tool registry not poisoned");
        registry.closed = true;
        registry.handlers.clear();
    }
    /// Wait until this connection is marked closed, without polling.
    ///
    /// This does not wait for task destruction or acknowledge Server-side cleanup,
    /// and does not consume buffered [`Events`]. Canceling this wait has no effect.
    pub async fn closed(&self) {
        self.0.peer.closed().await;
    }
    /// Typed escape hatch for protocol extensions; does not expose wire peer types.
    ///
    /// Canceling the future releases its correlation slot, but cannot retract an
    /// already-sent command. Apply deadlines at the call site and do not blindly
    /// retry side effects after a lost response.
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T, Error> {
        let result = self
            .0
            .peer
            .request(method, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }
    /// Read the active resolved snapshot, never credential values.
    pub async fn config(&self) -> Result<ConfigSnapshot, Error> {
        self.request("config.get", Value::Null).await
    }
    /// Validate a fully resolved snapshot without changing the active revision.
    pub async fn validate_config(&self, config: &ResolvedConfig) -> Result<(), Error> {
        self.request("config.validate", config).await
    }
    /// Apply a compare-and-swap resolved configuration update.
    pub async fn apply_config(
        &self,
        base_revision: u64,
        config: ResolvedConfig,
    ) -> Result<ConfigSnapshot, Error> {
        self.request(
            "config.apply",
            ConfigApply {
                base_revision,
                config,
            },
        )
        .await
    }
    /// Populate the initial memory-only generic SecretStore over trusted local IPC.
    pub async fn put_secret(&self, key: &str, secret: &str) -> Result<(), Error> {
        self.request("secret.put", json!({"key":key, "value":secret}))
            .await
    }
    /// Allocate a session independent of this connection's lifetime.
    pub async fn create_session(&self) -> Result<SessionId, Error> {
        Ok(self
            .request::<SessionRef>("session.create", Value::Null)
            .await?
            .session_id)
    }
    /// Atomically replay events after `after_seq` and attach to live events.
    ///
    /// Returns the live head at attachment. Events may arrive before this response.
    /// Replay can redeliver events and is bounded by Server retention; track sequence
    /// numbers per session. Re-subscribing replaces this connection's subscription.
    pub async fn subscribe(&self, session_id: SessionId, after_seq: u64) -> Result<u64, Error> {
        #[derive(serde::Deserialize)]
        struct Head {
            live_head: u64,
        }
        Ok(self
            .request::<Head>(
                "subscribe",
                Subscribe {
                    session_id,
                    after_seq,
                },
            )
            .await?
            .live_head)
    }
    /// Stop future event delivery without canceling the session or run.
    /// Events already queued locally or in transit may still be received.
    pub async fn unsubscribe(&self, session_id: SessionId) -> Result<(), Error> {
        self.request("unsubscribe", SessionRef { session_id }).await
    }
    /// Submit a user message and authorize Server-owned inference.
    pub async fn start_run(&self, session_id: SessionId, message: String) -> Result<RunId, Error> {
        #[derive(serde::Deserialize)]
        struct Started {
            run_id: RunId,
        }
        Ok(self
            .request::<Started>(
                "run.start",
                RunStart {
                    session_id,
                    message,
                },
            )
            .await?
            .run_id)
    }
    /// Request a terminal cancellation transition for exactly this active run.
    pub async fn cancel_run(&self, session_id: SessionId, run_id: RunId) -> Result<(), Error> {
        self.request("run.cancel", RunCancel { session_id, run_id })
            .await
    }
    /// Replace this connection's tools for a session; an empty list unregisters them.
    ///
    /// The Server rejects changes during an active run. Registration is serialized
    /// across clones; callbacks are installed before the RPC to handle immediate
    /// reverse requests. On RPC failure the prior local registry is restored unless
    /// closed. Canceling after local installation closes the connection and releases
    /// callbacks: the Server's registration outcome cannot otherwise be determined.
    pub async fn register_tools(
        &self,
        session_id: SessionId,
        tools: Vec<Tool>,
    ) -> Result<ExecutorId, Error> {
        let _registration = self.0.registration.lock().await;
        let definitions = tools.iter().map(|tool| tool.definition.clone()).collect();
        let mut registering = Registering(Some(self));
        // Install first: the Server may issue a reverse request immediately after commit.
        let previous = {
            let mut registry = self.0.tools.write().expect("tool registry not poisoned");
            if registry.closed {
                return Err(transport::Error::Closed.into());
            }
            let handlers = &mut registry.handlers;
            let previous = handlers
                .iter()
                .filter(|((session, _), _)| *session == session_id)
                .map(|(key, handler)| (key.clone(), handler.clone()))
                .collect::<Vec<_>>();
            handlers.retain(|(session, _), _| *session != session_id);
            for tool in tools {
                handlers.insert((session_id, tool.definition.name), tool.handler);
            }
            previous
        };
        #[derive(serde::Deserialize)]
        struct Registered {
            executor_id: ExecutorId,
        }
        let result = match self
            .request::<Registered>(
                "tools.register",
                ToolsRegister {
                    session_id,
                    tools: definitions,
                },
            )
            .await
        {
            Ok(registered) => Ok(registered.executor_id),
            Err(error) => {
                let mut registry = self.0.tools.write().expect("tool registry not poisoned");
                // close() and rollback share this lock; rollback must never
                // resurrect callbacks (and captured Client cycles) after close.
                if !registry.closed {
                    registry
                        .handlers
                        .retain(|(session, _), _| *session != session_id);
                    registry.handlers.extend(previous);
                }
                Err(error)
            }
        };
        registering.0.take();
        result
    }
}
async fn execute(handlers: &Tools, method: &str, params: Value) -> Result<Value, ProtocolError> {
    if method != "tool.execute" {
        return Err(ProtocolError::new(
            "unknown_method",
            "unknown reverse method",
        ));
    }
    let request: ToolExecute = serde_json::from_value(params)
        .map_err(|_| ProtocolError::new("invalid_params", "invalid tool request"))?;
    let handler = handlers
        .read()
        .expect("tool registry not poisoned")
        .handlers
        .get(&(request.session_id, request.name))
        .cloned()
        .ok_or_else(|| ProtocolError::new("unknown_tool", "no matching Client tool"))?;
    let output = handler(request.arguments).await?;
    serde_json::to_value(ToolResult {
        lease: request.lease,
        output,
    })
    .map_err(|_| ProtocolError::new("internal", "cannot serialize tool output"))
}
