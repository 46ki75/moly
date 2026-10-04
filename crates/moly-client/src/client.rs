//! Semantic Client implementation behind the SDK's public facade.
use crate::transport::{self, Incoming, Peer};
use moly_protocol::{auth::*, *};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock, Weak},
};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;

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
type InteractionFuture =
    Pin<Box<dyn Future<Output = Result<InteractionOutcome, ProtocolError>> + Send>>;
type InteractionHandler = dyn Fn(InteractionRequest) -> InteractionFuture + Send + Sync;
struct Attempt {
    handler: Option<Weak<InteractionHandler>>,
    cancelled: CancellationToken,
}
#[derive(Default)]
struct AuthRegistry {
    attempts: HashMap<AuthAttemptId, Attempt>,
    closed: bool,
}
type AuthHandlers = Arc<RwLock<AuthRegistry>>;
struct Authenticating {
    registry: AuthHandlers,
    attempt_id: AuthAttemptId,
    peer: Peer,
    pending: bool,
}
impl Drop for Authenticating {
    fn drop(&mut self) {
        if let Some(attempt) = self
            .registry
            .write()
            .expect("auth registry not poisoned")
            .attempts
            .remove(&self.attempt_id)
        {
            attempt.cancelled.cancel();
        }
        if self.pending {
            // Dropping an auth future withdraws presentation authority immediately.
            // Cancel the remote operation best-effort without capturing a Client.
            let peer = self.peer.clone();
            let attempt_id = self.attempt_id;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        peer.request("auth.cancel", json!({"attempt_id": attempt_id})),
                    ).await;
                    if !matches!(result, Ok(Ok(Value::Null))) &&
                        !matches!(result, Ok(Err(transport::Error::Remote(ref error))) if error.code == "not_active") {
                        // If cancellation cannot be delivered/acknowledged, closing
                        // the initiating connection is the Server's cleanup fence.
                        peer.close();
                    }
                });
            } else {
                peer.close();
            }
        }
    }
}
struct Inner {
    peer: Peer,
    tools: Tools,
    auth: AuthHandlers,
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
    /// Return known execution failures as `Ok(json!({"error": {"code": ..., "message": ...}}))`
    /// with sanitized descriptions so the model can recover. `Err(ProtocolError)`
    /// is an RPC failure and terminates the run; the SDK does not reclassify it.
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
/// A headless application's asynchronous presentation callback for one auth attempt.
///
/// The SDK binds the response to the request identity. `Opened` means presented,
/// not authenticated. Do not log URLs or launch shell commands. A handler may issue
/// nested requests on the same Client. Unlike persistent tool registrations, auth
/// registrations are weak and scoped to [`Client::authenticate`]; capturing a Client
/// does not leave a registry ownership cycle after completion or cancellation.
pub struct Interaction {
    handler: Arc<InteractionHandler>,
}
impl Interaction {
    /// Define presentation policy without installing it globally on the connection.
    pub fn new<F, Fut>(present: F) -> Self
    where
        F: Fn(InteractionRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<InteractionOutcome, ProtocolError>> + Send + 'static,
    {
        Self {
            handler: Arc::new(move |request| Box::pin(present(request))),
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
        let auth: AuthHandlers = Arc::default();
        let interactions = auth.clone();
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
                            let interactions = interactions.clone();
                            requests.spawn(async move {
                                let result = if method == "interaction.request" {
                                    interact(&interactions, params).await
                                } else {
                                    execute(&handlers, &method, params).await
                                };
                                let _ = peer.respond(id, result).await;
                            });
                        }
                    }
                }
            }
            dispatcher.close();
            close_auth(&interactions);
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
                auth,
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
        close_auth(&self.0.auth);
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
    /// Perform an explicit Provider auth operation pinned to a config revision.
    ///
    /// Use a fresh attempt ID. An optional callback is installed before sending the
    /// command, and only a matching live `Login` can invoke it. Headless/default
    /// Clients return `interaction_unavailable`. No implicit human-login deadline,
    /// browser launch, process management, configuration, or persistence is provided.
    /// The callback is removed on every exit; dropping this future also aborts its
    /// callback futures and sends best-effort `auth.cancel`. For confirmed cancellation,
    /// call [`Client::cancel_auth`] concurrently and await this operation's terminal
    /// response. Lost responses remain uncertain; inspect status rather than retrying.
    pub async fn authenticate(
        &self,
        command: AuthCommand,
        interaction: Option<Interaction>,
    ) -> Result<AuthStatus, Error> {
        let attempt_id = command.attempt_id;
        {
            let mut registry = self.0.auth.write().expect("auth registry not poisoned");
            if registry.closed {
                return Err(Error::Closed);
            }
            if registry.attempts.contains_key(&attempt_id) {
                return Err(
                    ProtocolError::new("invalid_params", "auth attempt already active").into(),
                );
            }
            registry.attempts.insert(
                attempt_id,
                Attempt {
                    handler: if command.operation == AuthOperation::Login {
                        interaction
                            .as_ref()
                            .map(|interaction| Arc::downgrade(&interaction.handler))
                    } else {
                        None
                    },
                    cancelled: CancellationToken::new(),
                },
            );
        }
        let mut guard = Authenticating {
            registry: self.0.auth.clone(),
            attempt_id,
            peer: self.0.peer.clone(),
            pending: true,
        };
        let result = self.request::<AuthStatus>("provider.auth", command).await;
        guard.pending = false;
        // The operation owns the strong callback; the registry never does. Keep it
        // alive across the RPC even when the compiler can see no further use.
        drop(interaction);
        match result {
            Ok(status) if status.attempt_id != attempt_id => {
                self.close();
                Err(Error::Frame("mismatched auth attempt"))
            }
            result => result,
        }
    }
    /// Cancel an auth operation on this same connection, not an unrelated run.
    ///
    /// Await the original auth response as well: this acknowledgment alone does not
    /// establish whether completion won the race or recover a lost auth result.
    /// Once the cancel RPC returns, matching presentation callbacks are withdrawn,
    /// while the original auth request remains pending until its terminal response.
    pub async fn cancel_auth(&self, attempt_id: AuthAttemptId) -> Result<(), Error> {
        let result = self.request("auth.cancel", AuthCancel { attempt_id }).await;
        if let Some(attempt) = self
            .0
            .auth
            .write()
            .expect("auth registry not poisoned")
            .attempts
            .get_mut(&attempt_id)
        {
            attempt.handler = None;
            attempt.cancelled.cancel();
        }
        result
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
fn close_auth(handlers: &AuthHandlers) {
    let mut registry = handlers.write().expect("auth registry not poisoned");
    registry.closed = true;
    for (_, attempt) in registry.attempts.drain() {
        attempt.cancelled.cancel();
    }
}
async fn interact(handlers: &AuthHandlers, params: Value) -> Result<Value, ProtocolError> {
    let request: InteractionRequest = serde_json::from_value(params)
        .map_err(|_| ProtocolError::new("invalid_params", "invalid interaction request"))?;
    let attempt_id = request.attempt_id;
    let unavailable =
        || ProtocolError::new("interaction_unavailable", "no live Client interaction");
    let (handler, cancelled) = {
        let registry = handlers.read().expect("auth registry not poisoned");
        let attempt = registry.attempts.get(&attempt_id).ok_or_else(unavailable)?;
        let handler = attempt
            .handler
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(unavailable)?;
        (handler, attempt.cancelled.clone())
    };
    let outcome = tokio::select! {
        biased;
        _ = cancelled.cancelled() => return Err(unavailable()),
        result = handler(request) => result?,
    };
    if cancelled.is_cancelled() {
        return Err(unavailable());
    }
    serde_json::to_value(InteractionResponse {
        attempt_id,
        outcome,
    })
    .map_err(|_| ProtocolError::new("internal", "cannot serialize interaction outcome"))
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
