//! Lazy REPL client: local input and commands precede runtime/backend creation.
mod backend;
mod repl;

use std::process::ExitCode;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Client(#[from] moly_client::Error),
    #[error("Server request timed out; outcome is unknown; no automatic retry")]
    Timeout(#[from] tokio::time::error::Elapsed),
    #[error("could not start moly-server; build/install both binaries or use --connect")]
    Spawn(#[source] std::io::Error),
    #[error("Server failed to report readiness")]
    Readiness,
    #[error("invalid_config: MOLY_PROVIDER must be openai or opencode-go")]
    ProviderProfile,
    #[error("invalid_config: {0} must contain valid Unicode")]
    InvalidEnvironment(&'static str),
    #[error("Server disconnected; run outcome may be unknown; reconnect explicitly")]
    Disconnected,
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let endpoint = match args.as_slice() {
        [] => None,
        [flag, endpoint] if flag == "--connect" => Some(endpoint.clone()),
        [flag] if matches!(flag.as_str(), "--help" | "-h") => {
            println!("usage: moly [--connect <local endpoint>]\n\n{}", repl::HELP);
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("usage: moly [--connect <local endpoint>]\nTry --help for REPL commands.");
            return ExitCode::from(2);
        }
    };
    match start(endpoint) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn start(endpoint: Option<String>) -> Result<(), Error> {
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
        .block_on(repl::run(endpoint, first))
}
