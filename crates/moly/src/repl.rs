//! Line-oriented UI; all authoritative state remains behind the SDK.
use crate::{
    Error,
    backend::{Backend, rpc, spawn_server},
};
use moly_client::protocol::EventKind;
use std::io::{self, BufRead, Write};
use tokio::sync::mpsc;

pub(crate) const HELP: &str = "Enter a message to run it in the current conversation.
/help       Show this help (no backend needed)
/new        Start a fresh conversation on the next message
/quit       Exit the CLI; leave the Server running (/exit also works)
//text      Send a message starting with / instead of a command
Ctrl-C cancels an active run; at an idle prompt it exits. EOF exits.
Input entered during a run is processed after that run finishes.";

#[derive(Debug, PartialEq, Eq)]
enum Input<'a> {
    Empty,
    Help,
    New,
    Quit,
    Unknown,
    Message(&'a str),
}

fn parse(line: &str) -> Input<'_> {
    let trimmed = line.trim();
    match trimmed {
        "" => Input::Empty,
        "/help" => Input::Help,
        "/new" => Input::New,
        "/quit" | "/exit" => Input::Quit,
        _ if trimmed.starts_with("//") => Input::Message(&trimmed[1..]),
        _ if trimmed.starts_with('/') => Input::Unknown,
        _ => Input::Message(line.trim_end()),
    }
}

fn prompt() -> io::Result<()> {
    print!("moly> ");
    io::stdout().flush()
}

fn local(input: Input<'_>) {
    match input {
        Input::Help => println!("{HELP}"),
        Input::New => println!("New conversation on the next message."),
        Input::Unknown => eprintln!("Unknown command. Use /help, or // to send a literal slash."),
        _ => {}
    }
}

pub(crate) fn first_message() -> io::Result<Option<String>> {
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
            Input::Message(message) => return Ok(Some(message.into())),
            input => local(input),
        }
    }
}

fn input_lines() -> io::Result<mpsc::Receiver<io::Result<String>>> {
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

pub(crate) async fn run(mut endpoint: Option<String>, first: String) -> Result<(), Error> {
    let mut lines = input_lines()?;
    let mut backend: Option<Backend> = None;
    let mut line = first;
    // The first line is already parsed, including any literal-slash escape.
    let mut input = Input::Message(&line);
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

async fn submit(
    endpoint: &mut Option<String>,
    backend: &mut Option<Backend>,
    message: &str,
) -> Result<(), Error> {
    if endpoint.is_none() {
        *endpoint = Some(spawn_server().await?);
    }
    if backend.is_none() {
        *backend = Some(Backend::connect(endpoint.as_deref().expect("endpoint resolved")).await?);
    }
    // Keep the connection after configuration errors; another Client may repair it.
    // In particular, retrying setup must not spawn a second unmanaged Server.
    let backend = backend.as_mut().expect("connection established");
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
