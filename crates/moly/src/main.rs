//! Lazy REPL client: local input and commands precede runtime/backend creation.
mod auth_state;
mod backend;
mod direct;
mod repl;

use std::process::ExitCode;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Client(#[from] moly_client::Error),
    #[error(transparent)]
    Provider(#[from] moly_provider_client::protocol::ProtocolError),
    #[error("Agent Server request timed out; outcome is unknown; no automatic retry")]
    Timeout(#[from] tokio::time::error::Elapsed),
    #[error("could not start moly-server; build/install both binaries or use --connect")]
    Spawn(#[source] std::io::Error),
    #[error("Agent Server failed to report readiness")]
    Readiness,
    #[error("invalid_config: MOLY_PROVIDER must be openai, opencode-go, or openai-codex")]
    ProviderProfile,
    #[error("invalid_config: {0}")]
    InvalidConfig(&'static str),
    #[error("auth_state: {0}")]
    AuthState(&'static str),
    #[error("invalid_config: {0} must contain valid Unicode")]
    InvalidEnvironment(&'static str),
    #[error("Agent Server disconnected; run outcome may be unknown; reconnect explicitly")]
    Disconnected,
}

enum Mode {
    Server(Option<String>),
    Direct,
}

const USAGE: &str = "usage: moly [--connect <local endpoint> | --direct]";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mode = match args.as_slice() {
        [] => Mode::Server(None),
        [flag] if flag == "--direct" => Mode::Direct,
        [flag, endpoint] if flag == "--connect" => Mode::Server(Some(endpoint.clone())),
        [flag] if matches!(flag.as_str(), "--help" | "-h") => {
            println!("{USAGE}\n\n{}", repl::HELP);
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}\nTry --help for REPL commands.");
            return ExitCode::from(2);
        }
    };
    match start(mode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn start(mode: Mode) -> Result<(), Error> {
    let Some(first) = repl::first_message()? else {
        return Ok(());
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match mode {
                Mode::Server(endpoint) => repl::run(endpoint, first).await,
                Mode::Direct => direct::run(first).await,
            }
        })
}
