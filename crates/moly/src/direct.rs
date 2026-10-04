//! Opt-in, single-conversation MPP host. No Server, tool loop, or durable secrets.
use crate::{
    Error,
    backend::{ResolvedProvider, resolve_provider},
    repl::{self, First, Input},
};
use moly_provider_client::{
    Interaction, ProviderClient,
    protocol::{
        AuthAttemptId, ModelCallId, ProtocolError, RunId, SessionId,
        auth::{AuthOperation, AuthStatus, ProviderAuthRequest},
        model::{CallKind, InferenceContext, ModelMessage, ModelRequest, ModelStep},
    },
};
use std::io;
use tokio::sync::mpsc;

struct Direct {
    provider: ResolvedProvider,
    client: ProviderClient,
    validated: bool,
    session: Option<SessionId>,
    messages: Vec<ModelMessage>,
}

impl Direct {
    fn resolve() -> Result<Self, Error> {
        Ok(Self {
            provider: resolve_provider()?,
            client: ProviderClient,
            validated: false,
            session: None,
            messages: vec![],
        })
    }

    async fn validate(&mut self) -> Result<(), Error> {
        if !self.validated {
            self.client.validate(&self.provider.config).await?;
            self.validated = true;
        }
        Ok(())
    }

    async fn authenticate(&mut self, operation: AuthOperation) -> Result<AuthStatus, Error> {
        self.validate().await?;
        if !self.provider.credential_scope {
            return Err(ProtocolError::new(
                "not_configured",
                "Authentication requires a selected credential scope",
            )
            .into());
        }
        let interaction = (operation == AuthOperation::Login).then(|| {
            Interaction::new(|request| async move { repl::present_auth_url(&request.url) })
        });
        let status = self
            .client
            .authenticate(
                &self.provider.config,
                ProviderAuthRequest {
                    attempt_id: AuthAttemptId::new(),
                    operation,
                    options: serde_json::Value::Null,
                    credential: None,
                },
                &mut self.provider.credential,
                interaction,
            )
            .await?;
        if let (Some(state), Some(registration)) =
            (&mut self.provider.auth_state, &status.registration)
        {
            // The same owner-only, nonsecret format and conflict checks as Server
            // mode. Persist before changing the in-memory Provider options.
            state.persist(registration.clone())?;
            self.provider.config.options["registration"] = registration.clone();
        }
        Ok(status)
    }

    async fn submit(&mut self, message: &str) -> Result<(), Error> {
        self.validate().await?;
        let session_id = *self.session.get_or_insert_with(SessionId::new);
        let mut working = self.messages.clone();
        working.push(ModelMessage::User {
            text: message.into(),
        });
        let result = self
            .client
            .step(
                &self.provider.config,
                ModelRequest {
                    options: serde_json::Value::Null,
                    credential: None,
                    context: InferenceContext {
                        session_id,
                        run_id: RunId::new(),
                        model_call_id: ModelCallId::new(),
                        call_kind: CallKind::Primary,
                    },
                    messages: working.clone(),
                    tools: vec![],
                },
                self.provider
                    .credential_scope
                    .then_some(&mut self.provider.credential),
            )
            .await?;
        match result {
            ModelStep::Completed { text, metadata } => {
                working.push(ModelMessage::Assistant {
                    text: Some(text.clone()),
                    tool_calls: vec![],
                    metadata,
                });
                // Only success promotes context. Dropping this future discards the
                // whole working turn, but never rolls back an SDK credential rotation.
                self.messages = working;
                println!("{text}");
                Ok(())
            }
            ModelStep::AwaitHostTools { .. } => Err(ProtocolError::new(
                "invalid_tool",
                "Direct mode does not advertise or execute tools",
            )
            .into()),
        }
    }

    fn new_session(&mut self) {
        self.session = None;
        self.messages.clear();
    }
}

pub(crate) async fn run(first: First) -> Result<(), Error> {
    let mut lines = repl::input_lines()?;
    let mut backend: Option<Direct> = None;
    let (mut line, operation) = match first {
        First::Message(message) => (message, None),
        First::Auth(operation) => (String::new(), Some(operation)),
    };
    // First already distinguishes literal // commands; do not parse it again.
    let mut input = operation.map(Input::Auth).unwrap_or(Input::Message(&line));
    loop {
        let result = match input {
            Input::Quit => break,
            Input::New => {
                if let Some(backend) = &mut backend {
                    backend.new_session();
                }
                repl::local(Input::New);
                Ok(())
            }
            Input::Message(message) => match ensure_backend(&mut backend) {
                Ok(backend) => {
                    // Do not consume stdin here: messages and EOF queued during
                    // inference are processed only after completion/cancellation.
                    tokio::select! {
                        biased;
                        result = backend.submit(message) => result,
                        signal = tokio::signal::ctrl_c() => {
                            signal?;
                            eprintln!("run cancelled");
                            Ok(())
                        }
                    }
                }
                Err(error) => Err(error),
            },
            Input::Auth(operation) => match ensure_backend(&mut backend) {
                Ok(backend) => {
                    if authenticate(backend, operation, &mut lines).await? {
                        break;
                    }
                    Ok(())
                }
                Err(error) => Err(error),
            },
            input => {
                repl::local(input);
                Ok(())
            }
        };
        if let Err(error) = result {
            eprintln!("error: {error}");
        }
        repl::prompt()?;
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                println!();
                return Ok(());
            }
            next = lines.recv() => {
                let Some(next) = next else { return Ok(()); };
                line = next?;
            }
        }
        input = repl::parse(&line);
    }
    Ok(())
}

fn ensure_backend(backend: &mut Option<Direct>) -> Result<&mut Direct, Error> {
    if backend.is_none() {
        *backend = Some(Direct::resolve()?);
    }
    Ok(backend.as_mut().expect("direct configuration resolved"))
}

async fn authenticate(
    backend: &mut Direct,
    operation: AuthOperation,
    lines: &mut mpsc::Receiver<io::Result<String>>,
) -> Result<bool, Error> {
    // Pin only within this block. Returning to the prompt/exit drops the SDK
    // operation and its owned child before the credential slot can be used again.
    let result = {
        let auth = backend.authenticate(operation);
        tokio::pin!(auth);
        loop {
            tokio::select! {
                biased;
                result = &mut auth => break Some(result),
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    eprintln!("Authentication cancelled.");
                    break None;
                }
                next = lines.recv() => {
                    match next {
                        None => return Ok(true),
                        Some(Err(error)) => return Err(error.into()),
                        Some(Ok(line)) if repl::parse(&line) == Input::Quit => return Ok(true),
                        Some(Ok(_)) => eprintln!("Authentication active; use Ctrl-C to cancel. Other input ignored."),
                    }
                }
            }
        }
    };
    match result {
        Some(Ok(status)) => repl::show_auth_status(operation, &status),
        Some(Err(error)) => eprintln!("error: {error}"),
        None => {}
    }
    Ok(false)
}
