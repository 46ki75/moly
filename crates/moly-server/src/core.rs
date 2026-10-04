//! Server Core: live-state authority without config discovery or transport I/O.
use crate::executors::LocalExecutor;
use crate::{model_provider::ModelProvider, secrets::SecretStore};
use moly_protocol::model::{
    CallKind, HostToolCall, InferenceContext, ModelMessage, ModelRequest, ModelStep,
};
use moly_protocol::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAILBOX: usize = 128;
const RETAINED_EVENTS: usize = 1024;
const MAX_MODEL_STEPS: usize = 16;
/// Bounded effects delivered to the connection adapter, not a transport API.
pub enum Output {
    /// A canonical event already committed by the session actor.
    Event(SessionEvent),
    /// Reverse request for a Client-hosted side effect.
    Tool {
        /// Server-issued lease and arguments.
        request: ToolExecute,
        /// Only a matching lease may be committed.
        reply: oneshot::Sender<Result<ToolResult, ProtocolError>>,
    },
}
/// An attached peer's capabilities, independent of sessions and runs.
#[derive(Clone)]
pub struct Connection {
    id: ConnectionId,
    executor_id: ExecutorId,
    output: mpsc::Sender<Output>,
    closed: CancellationToken,
}
impl Connection {
    /// Create a bounded effect channel for an ordinary Client of any role.
    pub fn new() -> (Self, mpsc::Receiver<Output>) {
        let (output, rx) = mpsc::channel(RETAINED_EVENTS + MAILBOX);
        (
            Self {
                id: ConnectionId::new(),
                executor_id: ExecutorId::new(),
                output,
                closed: CancellationToken::new(),
            },
            rx,
        )
    }
    /// Correlate diagnostics without exposing user payloads.
    pub fn id(&self) -> ConnectionId {
        self.id
    }
    /// Signal disconnection/slow-consumer failure, never cancel a session.
    pub fn close(&self) {
        self.closed.cancel();
    }
    /// Wait for a connection-scoped failure.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }
    fn emit(&self, output: Output) -> Result<(), ProtocolError> {
        if self.closed.is_cancelled() || self.output.try_send(output).is_err() {
            self.close();
            return Err(error(
                "slow_consumer",
                "connection unavailable or output queue exhausted",
            ));
        }
        Ok(())
    }
}
struct Shared {
    id: ServerId,
    config: Mutex<ConfigSnapshot>,
    sessions: Mutex<HashMap<SessionId, mpsc::Sender<Command>>>,
    provider: Arc<ModelProvider>,
    executor: Option<Arc<LocalExecutor>>,
    secrets: SecretStore,
    shutdown: CancellationToken,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
/// Cloneable command router. Each session serializes its own mutations.
#[derive(Clone)]
pub struct Server(Arc<Shared>);
impl Server {
    /// Convenience composition for in-process state-machine tests.
    #[cfg(test)]
    pub fn new() -> Result<Self, ProtocolError> {
        Self::with_local_executor(Some(LocalExecutor::new()))
    }
    /// Compose live authority with an optional local executor; read no configuration.
    /// Without it, hosted tools are supplied entirely by ordinary Clients.
    pub fn with_local_executor(executor: Option<LocalExecutor>) -> Result<Self, ProtocolError> {
        Ok(Self(Arc::new(Shared {
            id: ServerId::new(),
            config: Mutex::new(ConfigSnapshot {
                revision: 0,
                config: None,
            }),
            sessions: Mutex::new(HashMap::new()),
            provider: Arc::new(ModelProvider),
            executor: executor.map(Arc::new),
            secrets: SecretStore::default(),
            shutdown: CancellationToken::new(),
        })))
    }
    /// Stable for this live Server incarnation, unrelated to process or endpoint.
    pub fn id(&self) -> ServerId {
        self.0.id
    }
    /// Abort live work on explicit Server shutdown, not Client disconnection.
    pub fn shutdown(&self) {
        self.0.shutdown.cancel();
    }
    /// Dispatch an initialized peer's operation. The adapter owns the handshake gate.
    #[tracing::instrument(skip_all, fields(server_id = %self.id(), connection_id = %connection.id()))]
    pub async fn request(
        &self,
        connection: &Connection,
        method: &str,
        params: Value,
    ) -> Result<Value, ProtocolError> {
        match method {
            "config.get" => {
                return value(
                    self.0
                        .config
                        .lock()
                        .expect("config lock not poisoned")
                        .clone(),
                );
            }
            "config.validate" => {
                let config = decode::<ResolvedConfig>(params)?;
                validate_config(&config)?;
                self.0.provider.validate(&config.provider).await?;
                return Ok(Value::Null);
            }
            "config.apply" => {
                let apply: ConfigApply = decode(params)?;
                validate_config(&apply.config)?;
                // Reject a known stale update before executing its configured program.
                // Recheck after asynchronous validation: another Client may win CAS.
                if self
                    .0
                    .config
                    .lock()
                    .expect("config lock not poisoned")
                    .revision
                    != apply.base_revision
                {
                    return Err(error("revision_conflict", "configuration revision changed"));
                }
                self.0.provider.validate(&apply.config.provider).await?;
                let mut config = self.0.config.lock().expect("config lock not poisoned");
                if config.revision != apply.base_revision {
                    return Err(error("revision_conflict", "configuration revision changed"));
                }
                config.revision = config
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| error("limit", "revision exhausted"))?;
                config.config = Some(apply.config);
                return value(config.clone());
            }
            "secret.put" => {
                #[derive(serde::Deserialize)]
                struct Put {
                    key: String,
                    value: String,
                }
                let put: Put = decode(params)?;
                if put.key.is_empty() || put.key.len() > 128 || put.value.len() > 16 * 1024 {
                    return Err(error("invalid_params", "invalid secret size or key"));
                }
                self.0.secrets.insert(put.key, put.value);
                return Ok(Value::Null);
            }
            "session.create" => {
                let id = SessionId::new();
                let (tx, rx) = mpsc::channel(MAILBOX);
                self.0
                    .sessions
                    .lock()
                    .expect("session map not poisoned")
                    .insert(id, tx.clone());
                let actor = Session {
                    id,
                    seq: 0,
                    events: VecDeque::new(),
                    subscribers: HashMap::new(),
                    registered: HashMap::new(),
                    messages: Vec::new(),
                    active: None,
                    provider: self.0.provider.clone(),
                    executor: self.0.executor.clone(),
                    secrets: self.0.secrets.clone(),
                    tx,
                    shutdown: self.0.shutdown.clone(),
                };
                tokio::spawn(actor.serve(rx));
                return value(SessionRef { session_id: id });
            }
            _ => {}
        }
        if !matches!(
            method,
            "run.start" | "run.cancel" | "subscribe" | "unsubscribe" | "tools.register"
        ) {
            return Err(error("unknown_method", "unknown method"));
        }
        let target: SessionRef = decode(params.clone())?;
        let tx = self
            .0
            .sessions
            .lock()
            .expect("session map not poisoned")
            .get(&target.session_id)
            .cloned()
            .ok_or_else(|| error("not_found", "unknown session"))?;
        let config = self
            .0
            .config
            .lock()
            .expect("config lock not poisoned")
            .clone();
        let (reply, rx) = oneshot::channel();
        tx.send(Command::Request {
            connection: connection.clone(),
            method: method.into(),
            params,
            config,
            reply,
        })
        .await
        .map_err(|_| error("closed", "session stopped"))?;
        rx.await.map_err(|_| error("closed", "session stopped"))?
    }
    /// Remove connection-lifetime registrations and subscriptions, not live runs.
    pub async fn disconnect(&self, connection: &Connection) {
        connection.close();
        let sessions: Vec<_> = self
            .0
            .sessions
            .lock()
            .expect("session map not poisoned")
            .values()
            .cloned()
            .collect();
        for tx in sessions {
            let _ = tx.send(Command::Disconnect(connection.id)).await;
        }
    }
}
fn error(code: &str, message: &str) -> ProtocolError {
    ProtocolError::new(code, message)
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(value)
        .map_err(|_| error("invalid_params", "invalid operation parameters"))
}
fn value(value: impl Serialize) -> Result<Value, ProtocolError> {
    serde_json::to_value(value).map_err(|_| error("internal", "serialization failed"))
}
fn validate_config(config: &ResolvedConfig) -> Result<(), ProtocolError> {
    if !std::path::Path::new(&config.workspace).is_absolute()
        || config
            .secret_ref
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.len() > 128)
    {
        return Err(error(
            "invalid_config",
            "expected absolute workspace and valid secret reference",
        ));
    }
    Ok(())
}
enum Command {
    Request {
        connection: Connection,
        method: String,
        params: Value,
        config: ConfigSnapshot,
        reply: oneshot::Sender<Result<Value, ProtocolError>>,
    },
    Disconnect(ConnectionId),
    ModelDone(RunId, Result<ModelStep, ProtocolError>),
    ToolDone(RunId, ToolLease, Result<ToolResult, ProtocolError>),
}
#[derive(Clone)]
struct Registered {
    definition: ToolDefinition,
    connection: Connection,
}
struct Active {
    id: RunId,
    config: ResolvedConfig,
    messages: Vec<ModelMessage>,
    tools: Vec<ToolDefinition>,
    pending_tools: VecDeque<HostToolCall>,
    expected: Option<(ToolLease, String)>,
    steps: usize,
    cancel: CancellationToken,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
struct Session {
    id: SessionId,
    seq: u64,
    events: VecDeque<SessionEvent>,
    subscribers: HashMap<ConnectionId, Connection>,
    registered: HashMap<String, Registered>,
    messages: Vec<ModelMessage>,
    active: Option<Active>,
    provider: Arc<ModelProvider>,
    executor: Option<Arc<LocalExecutor>>,
    secrets: SecretStore,
    tx: mpsc::Sender<Command>,
    shutdown: CancellationToken,
}
impl Session {
    async fn serve(mut self, mut rx: mpsc::Receiver<Command>) {
        self.commit(EventKind::SessionCreated);
        loop {
            let command = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                command = rx.recv() => match command { Some(command) => command, None => break },
            };
            match command {
                Command::Request {
                    connection,
                    method,
                    params,
                    config,
                    reply,
                } => {
                    let result = self.handle(connection, &method, params, config);
                    let _ = reply.send(result);
                }
                Command::Disconnect(id) => {
                    self.subscribers.remove(&id);
                    self.registered.retain(|_, tool| tool.connection.id != id);
                }
                Command::ModelDone(run, result) if self.is_active(run) => self.model_done(result),
                Command::ToolDone(run, lease, result) if self.is_active(run) => {
                    self.tool_done(lease, result)
                }
                _ => {} // Completion of cancelled/obsolete work cannot mutate the next run.
            }
        }
    }
    fn is_active(&self, run: RunId) -> bool {
        self.active.as_ref().is_some_and(|active| active.id == run)
    }
    fn commit(&mut self, kind: EventKind) {
        self.seq = self
            .seq
            .checked_add(1)
            .expect("session sequence space exhausted");
        let event = SessionEvent {
            session_id: self.id,
            seq: self.seq,
            kind,
        };
        tracing::debug!(session_id = %self.id, session_seq = self.seq, "session transition committed");
        self.events.push_back(event.clone());
        if self.events.len() > RETAINED_EVENTS {
            self.events.pop_front();
        }
        self.subscribers
            .retain(|_, connection| connection.emit(Output::Event(event.clone())).is_ok());
    }
    fn handle(
        &mut self,
        connection: Connection,
        method: &str,
        params: Value,
        snapshot: ConfigSnapshot,
    ) -> Result<Value, ProtocolError> {
        match method {
            "subscribe" => {
                let request: Subscribe = decode(params)?;
                if request.after_seq > self.seq {
                    return Err(error("invalid_seq", "sequence is ahead of live head"));
                }
                if self
                    .events
                    .front()
                    .is_some_and(|first| request.after_seq < first.seq - 1)
                {
                    return Err(error(
                        "replay_unavailable",
                        "requested events are no longer retained",
                    ));
                }
                // Replay plus attachment is one mailbox operation: no replay/live gap.
                for event in self
                    .events
                    .iter()
                    .filter(|event| event.seq > request.after_seq)
                {
                    connection.emit(Output::Event(event.clone()))?;
                }
                self.subscribers.insert(connection.id, connection);
                Ok(json!({"live_head": self.seq}))
            }
            "unsubscribe" => {
                self.subscribers.remove(&connection.id);
                Ok(Value::Null)
            }
            "tools.register" => {
                if self.active.is_some() {
                    return Err(error(
                        "run_busy",
                        "cannot replace tools during an active run",
                    ));
                }
                self.registered
                    .retain(|_, tool| !tool.connection.closed.is_cancelled());
                let request: ToolsRegister = decode(params)?;
                if request.tools.len() > 32 {
                    return Err(error("limit", "too many tools"));
                }
                let mut names = std::collections::HashSet::new();
                for tool in &request.tools {
                    if tool.name.is_empty()
                        || tool.name.len() > 64
                        || !tool
                            .name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                        || (self.executor.is_some() && tool.name == "read_file")
                        || !tool.input_schema.is_object()
                        || !names.insert(&tool.name)
                        || self
                            .registered
                            .get(&tool.name)
                            .is_some_and(|registered| registered.connection.id != connection.id)
                    {
                        return Err(error(
                            "invalid_tool",
                            "invalid, reserved, or occupied tool name/schema",
                        ));
                    }
                }
                self.registered
                    .retain(|_, tool| tool.connection.id != connection.id);
                for definition in request.tools {
                    self.registered.insert(
                        definition.name.clone(),
                        Registered {
                            definition,
                            connection: connection.clone(),
                        },
                    );
                }
                Ok(json!({"executor_id": connection.executor_id}))
            }
            "run.start" => {
                let request: RunStart = decode(params)?;
                if self.active.is_some() {
                    return Err(error("run_busy", "session already has an active run"));
                }
                if request.message.is_empty() || request.message.len() > 64 * 1024 {
                    return Err(error(
                        "invalid_params",
                        "message must contain 1..65536 bytes",
                    ));
                }
                let config = snapshot
                    .config
                    .ok_or_else(|| error("not_configured", "apply resolved configuration first"))?;
                let id = RunId::new();
                self.messages.push(ModelMessage::User {
                    text: request.message.clone(),
                });
                self.commit(EventKind::MessageAccepted {
                    message: request.message,
                });
                self.commit(EventKind::RunStarted {
                    run_id: id,
                    config_revision: snapshot.revision,
                });
                let mut tools: Vec<_> = self
                    .executor
                    .iter()
                    .map(|_| LocalExecutor::definition())
                    .collect();
                let mut registered: Vec<_> = self
                    .registered
                    .values()
                    .map(|tool| tool.definition.clone())
                    .collect();
                registered.sort_by(|a, b| a.name.cmp(&b.name));
                tools.extend(registered);
                self.active = Some(Active {
                    id,
                    config,
                    messages: self.messages.clone(),
                    tools,
                    pending_tools: VecDeque::new(),
                    expected: None,
                    steps: 0,
                    cancel: CancellationToken::new(),
                });
                self.start_model();
                Ok(json!({"run_id":id}))
            }
            "run.cancel" => {
                let request: RunCancel = decode(params)?;
                if !self.is_active(request.run_id) {
                    return Err(error("not_active", "run is not active"));
                }
                self.commit(EventKind::RunCancelled {
                    run_id: request.run_id,
                });
                self.active.take();
                Ok(Value::Null)
            }
            _ => Err(error("unknown_method", "unknown session operation")),
        }
    }
    fn start_model(&mut self) {
        let active = self
            .active
            .as_mut()
            .expect("model step requires active run");
        if active.steps >= MAX_MODEL_STEPS {
            self.fail(error("step_limit", "model step limit exceeded"));
            return;
        }
        active.steps += 1;
        let context = InferenceContext {
            session_id: self.id,
            run_id: active.id,
            model_call_id: ModelCallId::new(),
            call_kind: CallKind::Primary,
        };
        let run = active.id;
        let config = active.config.clone();
        let messages = active.messages.clone();
        let tools = active.tools.clone();
        let cancel = active.cancel.clone();
        self.commit(EventKind::ModelCallStarted {
            run_id: run,
            model_call_id: context.model_call_id,
        });
        let provider = self.provider.clone();
        let secrets = self.secrets.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                _ = async {
                    let credential = match config.secret_ref.as_deref() {
                        Some(key) => secrets.get(key).map(Some).ok_or_else(|| error("secret_not_found", "Model credential is unavailable")),
                        None => Ok(None),
                    };
                    let result = match credential {
                        Err(error) => Err(error),
                        Ok(credential) => provider.step(&config.provider, ModelRequest {
                            options: config.provider.options.clone(), credential, context, messages, tools,
                        }).await,
                    };
                    let _ = tx.send(Command::ModelDone(run, result)).await;
                } => {},
            }
        });
    }
    fn model_done(&mut self, result: Result<ModelStep, ProtocolError>) {
        match result {
            Err(error) => self.fail(error),
            Ok(ModelStep::Completed { text, metadata }) => {
                let active = self
                    .active
                    .as_mut()
                    .expect("completion requires active run");
                active.messages.push(ModelMessage::Assistant {
                    text: Some(text.clone()),
                    tool_calls: vec![],
                    metadata,
                });
                self.messages = active.messages.clone();
                let run_id = active.id;
                self.commit(EventKind::AssistantMessage { run_id, text });
                self.commit(EventKind::RunCompleted { run_id });
                self.active.take();
            }
            Ok(ModelStep::AwaitHostTools {
                text,
                calls,
                metadata,
            }) => {
                let active = self.active.as_mut().expect("tool step requires active run");
                if calls.is_empty() || calls.len() > 32 {
                    self.fail(error(
                        "provider_response",
                        "invalid number of host tool calls",
                    ));
                    return;
                }
                active.messages.push(ModelMessage::Assistant {
                    text,
                    tool_calls: calls.clone(),
                    metadata,
                });
                active.pending_tools = calls.into();
                self.start_tool();
            }
        }
    }
    fn start_tool(&mut self) {
        let active = self
            .active
            .as_mut()
            .expect("tool dispatch requires active run");
        let Some(call) = active.pending_tools.pop_front() else {
            self.start_model();
            return;
        };
        if !active.tools.iter().any(|tool| tool.name == call.name) {
            self.fail(error(
                "unknown_tool",
                "model requested an unadvertised tool",
            ));
            return;
        }
        let remote = self.registered.get(&call.name).cloned();
        let local = self.executor.clone().filter(|_| call.name == "read_file");
        let executor_id = if let Some(local) = &local {
            local.id()
        } else if let Some(remote) = &remote {
            remote.connection.executor_id
        } else {
            self.fail(error("executor_lost", "tool executor disconnected"));
            return;
        };
        let lease = ToolLease {
            tool_run_id: ToolRunId::new(),
            executor_id,
            generation: 1,
        };
        let request = ToolExecute {
            session_id: self.id,
            lease: lease.clone(),
            name: call.name.clone(),
            arguments: call.arguments,
        };
        active.expected = Some((lease.clone(), call.id));
        let run = active.id;
        let workspace = active.config.workspace.clone();
        let cancel = active.cancel.clone();
        self.commit(EventKind::ToolStarted {
            run_id: run,
            lease: lease.clone(),
            name: call.name,
        });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let work = async {
                let result = if let Some(remote) = remote {
                    let (reply, rx) = oneshot::channel();
                    match remote.connection.emit(Output::Tool { request, reply }) {
                        Err(error) => Err(error),
                        Ok(()) => tokio::select! {
                            biased;
                            _ = remote.connection.closed() => Err(error("executor_lost", "tool executor disconnected")),
                            result = rx => result.unwrap_or_else(|_| Err(error("executor_lost", "tool executor disconnected"))),
                        },
                    }
                } else if let Some(executor) = local {
                    executor.execute(&workspace, request).await
                } else {
                    Err(error("executor_lost", "no executor for this capability"))
                };
                let _ = tx.send(Command::ToolDone(run, lease, result)).await;
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                _ = work => {},
            }
        });
    }
    fn tool_done(&mut self, lease: ToolLease, result: Result<ToolResult, ProtocolError>) {
        let active = self
            .active
            .as_mut()
            .expect("tool completion requires active run");
        let Some((expected, call_id)) = active.expected.take() else {
            return;
        };
        if lease != expected {
            self.fail(error("stale_tool_result", "obsolete execution authority"));
            return;
        }
        match result {
            // Known execution failures arrive in output. Never infer recoverability
            // from an RPC error code or bypass lease validation for error results.
            Err(error) => self.fail(error),
            Ok(result) if result.lease != expected => self.fail(error(
                "stale_tool_result",
                "executor returned mismatched lease",
            )),
            Ok(result) => {
                let run_id = active.id;
                active.messages.push(ModelMessage::ToolResult {
                    call_id,
                    output: result.output,
                });
                self.commit(EventKind::ToolCompleted { run_id, lease });
                self.start_tool();
            }
        }
    }
    fn fail(&mut self, error: ProtocolError) {
        if let Some(active) = self.active.take() {
            self.commit(EventKind::RunFailed {
                run_id: active.id,
                error,
            });
        }
    }
}
