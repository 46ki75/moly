//! Shared line-oriented UI and the unchanged Agent Server-backed event loop.
use crate::{
    Error,
    backend::{Backend, rpc, spawn_server},
};
use moly_client::{
    Interaction,
    protocol::{AuthAttemptId, EventKind, ProtocolError, auth::*},
};
use std::io::{self, BufRead, Write};
use tokio::sync::mpsc;

pub(crate) const HELP: &str = "Enter a message to run it in the current conversation.
/help       Show this help (no backend needed)
/new        Start a fresh conversation on the next message
/login      Sign in using the configured Provider (lazy backend setup)
/auth       Show local auth status (/auth status also works)
/logout     Clear credentials and attempt revocation
/quit       Exit the CLI; any Agent Server stays running (/exit also works)
//text      Send a message starting with / instead of a command
Ctrl-C cancels an active run or login; at an idle prompt it exits. EOF exits.
Input entered during a run is processed after that run finishes.
During auth, EOF or /quit cancels and exits; other input is ignored.
OAuth pilot: MOLY_PROVIDER=openai-codex requires MOLY_MODEL and MOLY_AUTH_STATE_FILE.
URLs are presented for you to open; no browser is launched.
The Agent Server manages the agentic loop (executable: moly-server).
Tokens stay in host memory: Agent Server by default, CLI with --direct.
--direct explicitly hosts MPP without an Agent Server: no tools, replay, or shared sessions.";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Input<'a> {
    Empty,
    Help,
    New,
    Auth(AuthOperation),
    Quit,
    Unknown,
    Message(&'a str),
}

pub(crate) fn parse(line: &str) -> Input<'_> {
    let trimmed = line.trim();
    match trimmed {
        "" => Input::Empty,
        "/help" => Input::Help,
        "/new" => Input::New,
        "/login" => Input::Auth(AuthOperation::Login),
        "/auth" | "/auth status" => Input::Auth(AuthOperation::Status),
        "/logout" => Input::Auth(AuthOperation::Logout),
        "/quit" | "/exit" => Input::Quit,
        _ if trimmed.starts_with("//") => Input::Message(&trimmed[1..]),
        _ if trimmed.starts_with('/') => Input::Unknown,
        _ => Input::Message(line.trim_end()),
    }
}

pub(crate) fn prompt() -> io::Result<()> {
    print!("moly> ");
    io::stdout().flush()
}

pub(crate) fn local(input: Input<'_>) {
    match input {
        Input::Help => println!("{HELP}"),
        Input::New => println!("New conversation on the next message."),
        Input::Unknown => eprintln!("Unknown command. Use /help, or // to send a literal slash."),
        _ => {}
    }
}

pub(crate) enum First {
    Message(String),
    Auth(AuthOperation),
}
pub(crate) fn first_message() -> io::Result<Option<First>> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    loop {
        // Local commands, blank lines, and EOF need no runtime or config discovery.
        prompt()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        match parse(&line) {
            Input::Quit => return Ok(None),
            Input::Message(message) => return Ok(Some(First::Message(message.into()))),
            Input::Auth(operation) => return Ok(Some(First::Auth(operation))),
            input => local(input),
        }
    }
}

pub(crate) fn input_lines() -> io::Result<mpsc::Receiver<io::Result<String>>> {
    let (sender, receiver) = mpsc::channel(1);
    // Tokio stdin uses an uncancelable blocking task that can hold runtime shutdown
    // until another keystroke. A detached OS thread lets idle disconnect/Ctrl-C exit
    // while stdin is still open. At most two decoded lines wait ahead of the UI.
    std::thread::Builder::new()
        .name("moly-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                let failed = line.is_err();
                if sender.blocking_send(line).is_err() || failed {
                    break;
                }
            }
        })?;
    Ok(receiver)
}

pub(crate) async fn run(mut endpoint: Option<String>, first: First) -> Result<(), Error> {
    let mut lines = input_lines()?;
    let mut backend: Option<Backend> = None;
    let (mut line, operation) = match first {
        First::Message(message) => (message, None),
        First::Auth(operation) => (String::new(), Some(operation)),
    };
    // Never reparse the first message: //help must remain literal /help.
    let mut input = operation.map(Input::Auth).unwrap_or(Input::Message(&line));
    loop {
        let result = match input {
            Input::Quit => break,
            Input::New => {
                let result = match backend.as_mut() {
                    Some(backend) => backend.new_session().await,
                    None => Ok(()),
                };
                if result.is_ok() {
                    local(Input::New);
                }
                result
            }
            Input::Message(message) => submit(&mut endpoint, &mut backend, message).await,
            Input::Auth(operation) => {
                match authenticate(&mut endpoint, &mut backend, operation, &mut lines).await {
                    Ok(true) => break,
                    Ok(false) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            input => {
                local(input);
                Ok(())
            }
        };
        match result {
            // A timed-out command may already have committed. Never resubmit it.
            Err(error @ (Error::Timeout(_) | Error::Disconnected)) => return Err(error),
            Err(error) => eprintln!("error: {error}"),
            Ok(()) => {}
        }
        prompt()?;
        loop {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    println!();
                    return Ok(());
                }
                event = async {
                    match backend.as_mut() {
                        Some(backend) => backend.events.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Drain events from old runs while idle; only the submitted run
                    // is rendered below. Closure must be noticed without more input.
                    if event.is_none() { return Err(Error::Disconnected); }
                }
                next = lines.recv() => {
                    let Some(next) = next else { return Ok(()); };
                    line = next?;
                    break;
                }
            }
        }
        input = parse(&line);
    }
    Ok(())
}

async fn connect_backend<'a>(
    endpoint: &mut Option<String>,
    backend: &'a mut Option<Backend>,
) -> Result<&'a mut Backend, Error> {
    if endpoint.is_none() {
        *endpoint = Some(spawn_server().await?);
    }
    if backend.is_none() {
        *backend = Some(Backend::connect(endpoint.as_deref().expect("endpoint resolved")).await?);
    }
    // Keep the connection after configuration errors; another Client may repair it.
    // In particular, retrying setup must not spawn a second unmanaged Server.
    Ok(backend.as_mut().expect("connection established"))
}

async fn authenticate(
    endpoint: &mut Option<String>,
    backend: &mut Option<Backend>,
    operation: AuthOperation,
    lines: &mut mpsc::Receiver<io::Result<String>>,
) -> Result<bool, Error> {
    let backend = connect_backend(endpoint, backend).await?;
    let snapshot = backend.ensure_config().await?;
    let owned = backend.owns_auth_state(&snapshot)?;
    let attempt_id = AuthAttemptId::new();
    let interaction = (operation == AuthOperation::Login)
        .then(|| Interaction::new(|request| async move { present_auth_url(&request.url) }));
    let client = backend.client.clone();
    let auth = client.authenticate(
        AuthCommand {
            attempt_id,
            operation,
            config_revision: snapshot.revision,
        },
        interaction,
    );
    tokio::pin!(auth);
    let mut cancelling = false;
    let mut exiting = false;
    let mut input_error = None;
    let result = loop {
        tokio::select! {
            biased;
            // Poll/send auth first: already-buffered EOF or Ctrl-C must not send
            // auth.cancel before the original command exists on this connection.
            // Human login uses the Server's auth deadline, NOT the 10-second RPC
            // helper. Cancellation leaves this future alive until its terminal reply.
            result = &mut auth => break result,
            signal = tokio::signal::ctrl_c(), if !cancelling => {
                signal?;
                request_auth_cancel(&client, attempt_id).await?;
                cancelling = true;
            }
            next = lines.recv(), if !exiting => {
                match next {
                    None => exiting = true,
                    Some(Err(error)) => { input_error = Some(error); exiting = true; }
                    Some(Ok(line)) if parse(&line) == Input::Quit => exiting = true,
                    Some(Ok(_)) => eprintln!("Authentication active; use Ctrl-C to cancel. Other input ignored."),
                }
                if exiting && !cancelling {
                    request_auth_cancel(&client, attempt_id).await?;
                    cancelling = true;
                }
            }
            event = backend.events.recv() => {
                if event.is_none() { return Err(Error::Disconnected); }
            }
        }
    };
    if let Some(error) = input_error {
        return Err(error.into());
    }
    let completion = match result {
        Ok(mut status) => {
            let saved = backend
                .persist_registration(&snapshot, status.registration.take(), owned)
                .await;
            if saved.is_ok() {
                show_auth_status(operation, &status);
            }
            saved
        }
        Err(moly_client::Error::Remote(error)) if cancelling && error.code == "auth_cancelled" => {
            eprintln!("Authentication cancelled.");
            Ok(())
        }
        Err(error) => Err(error.into()),
    };
    if exiting {
        // EOF/quit remains an exit intention even when completion won the cancel
        // race with an error, or saving the nonsecret registration failed.
        if let Err(error) = completion {
            eprintln!("error: {error}");
        }
        return Ok(true);
    }
    completion?;
    Ok(false)
}
async fn request_auth_cancel(
    client: &moly_client::Client,
    attempt_id: AuthAttemptId,
) -> Result<(), Error> {
    match rpc(client.cancel_auth(attempt_id)).await {
        Ok(()) => Ok(()),
        // The auth terminal response still resolves the completion/cancel race.
        Err(Error::Client(moly_client::Error::Remote(error))) if error.code == "not_active" => {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn show_auth_status(operation: AuthOperation, status: &AuthStatus) {
    println!(
        "Authentication: {}",
        if status.authenticated {
            "signed in"
        } else {
            "signed out"
        }
    );
    if operation == AuthOperation::Logout && status.revocation_confirmed == Some(false) {
        println!("Upstream revocation was not confirmed.");
    }
}

pub(crate) fn present_auth_url(url: &str) -> Result<InteractionOutcome, ProtocolError> {
    if !safe_auth_url(url) {
        return Err(ProtocolError::new(
            "interaction_unavailable",
            "unsafe auth URL",
        ));
    }
    // Explicit UI only: never tracing, shell evaluation, or an automatic browser.
    println!("Open this HTTPS URL in your browser:\n{url}");
    io::stdout()
        .flush()
        .map_err(|_| ProtocolError::new("interaction_unavailable", "cannot present auth URL"))?;
    Ok(InteractionOutcome::Opened)
}

fn safe_auth_url(url: &str) -> bool {
    if url.len() > 8192
        || !url.is_ascii()
        || url.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        || url.contains('\\')
    {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if rest.contains('#') {
        return false;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    if host.is_empty()
        || !host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return false;
    }
    if port.is_some_and(|port| {
        !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().map_or(true, |port| port == 0)
    }) {
        return false;
    }
    if let Some((_, query)) = rest.split_once('?') {
        for part in query.split('&') {
            let key = part
                .split('=')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            // Encoded parameter names and token/hint-bearing URLs are not needed
            // by this pilot; reject rather than accidentally presenting credentials.
            if key.contains('%')
                || ["token", "secret", "password", "credential"]
                    .iter()
                    .any(|part| key.contains(part))
                || key == "code"
            {
                return false;
            }
        }
    }
    true
}

async fn submit(
    endpoint: &mut Option<String>,
    backend: &mut Option<Backend>,
    message: &str,
) -> Result<(), Error> {
    let backend = connect_backend(endpoint, backend).await?;
    let session = backend.session().await?;
    let run = rpc(backend.client.start_run(session, message.into())).await?;
    let mut cancel_requested = false;
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                if !cancel_requested {
                    match rpc(backend.client.cancel_run(session, run)).await {
                        Ok(()) => {},
                        // Completion may have won the race with Ctrl-C. Its terminal
                        // event is still authoritative and must be drained normally.
                        Err(Error::Client(moly_client::Error::Remote(error)))
                            if error.code == "not_active" => {},
                        Err(error) => return Err(error),
                    }
                    cancel_requested = true;
                }
            }
            event = backend.events.recv() => {
                let event = event.ok_or(Error::Disconnected)?;
                if event.session_id != session { continue; }
                match &event.kind {
                    EventKind::AssistantMessage { run_id, text } if *run_id == run => println!("{text}"),
                    EventKind::RunFailed { run_id, error } if *run_id == run => eprintln!("run failed: {error}"),
                    EventKind::RunCancelled { run_id } if *run_id == run => eprintln!("run cancelled"),
                    _ => {},
                }
                if event.kind.terminal_run() == Some(run) { return Ok(()); }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_and_literal_slashes_are_unambiguous() {
        for (text, operation) in [
            ("/login", AuthOperation::Login),
            ("/auth", AuthOperation::Status),
            (" /auth status\n", AuthOperation::Status),
            ("/logout", AuthOperation::Logout),
        ] {
            assert_eq!(parse(text), Input::Auth(operation));
        }
        for text in ["//login", "//help", "//auth", "//logout", "//new"] {
            assert_eq!(parse(text), Input::Message(&text[1..]));
        }
        assert_eq!(parse("/auth login"), Input::Unknown);
        assert_eq!(parse("/help"), Input::Help);
        assert_eq!(parse(" /new "), Input::New);
        assert_eq!(parse(" \n"), Input::Empty);
        assert_eq!(parse("/exit"), Input::Quit);
    }
    #[test]
    fn only_safe_https_authorization_urls_can_be_presented() {
        assert!(safe_auth_url(
            "https://auth.openai.com/authorize?response_type=code&state=opaque&code_challenge=pkce&redirect_uri=http%3A%2F%2F127.0.0.1%3A1234%2Fauth%2Fcallback"
        ));
        assert!(safe_auth_url("https://example.test:443/authorize"));
        for url in [
            "http://example.test",
            "javascript:alert(1)",
            "https://",
            "https://user@example.test",
            "https://example.test\\@evil.test",
            "https://example.test\n",
            "https://example.test/\u{1b}[2J",
            "https://example.test/#fragment",
            "https://example.test/?id_token_hint=private",
            "https://example.test/?access_token=private",
            "https://example.test/?%69d_token_hint=private",
            "https://example.test:0/",
            "https://example.test:abc/",
            "https://example.test:+443/",
            "https://example.test/\u{202e}hidden",
            "https://-bad.test/",
        ] {
            assert!(!safe_auth_url(url), "unsafe URL accepted");
        }
        assert!(!safe_auth_url(&format!(
            "https://example.test/{}",
            "x".repeat(8192)
        )));
    }
}
