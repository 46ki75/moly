//! Bundled OpenAI-compatible Model Provider v2 executable.

mod provider;
mod service;
mod transport;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Fixed logging policy prevents environment-selected dependency traces from
    // exposing HTTP details. Stdout is exclusively the protocol byte stream.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            tracing::error!("Could not start provider runtime");
            return ExitCode::FAILURE;
        }
    };
    let status = runtime.block_on(async {
        let service = match service::Service::new() {
            Ok(service) => service,
            Err(_) => {
                tracing::error!("Could not create HTTP provider");
                return ExitCode::FAILURE;
            }
        };
        match transport::serve(
            tokio::io::BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
            service,
        )
        .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => {
                tracing::warn!("Provider connection closed after transport failure");
                ExitCode::FAILURE
            }
        }
    });
    // Tokio's stdin uses a blocking read that cannot be interrupted. Do not let
    // runtime shutdown wait on it after an output failure or invalid input.
    runtime.shutdown_background();
    status
}
