//! Standalone direct Sign in with ChatGPT / Responses Model Provider v2.

mod config;
mod error;
mod http;
mod oauth;
mod responses;
mod service;
mod transport;

#[cfg(test)]
mod test_support;

use std::process::ExitCode;

fn main() -> ExitCode {
    // No env filter: dependency traces can contain URLs or authorization data.
    // Stdout is exclusively the protocol stream; diagnostics are static strings.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("off,moly_provider_openai_codex=warn")
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
                tracing::error!("Could not create provider");
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
                tracing::warn!("Provider transport closed");
                ExitCode::FAILURE
            }
        }
    });
    // Tokio stdin's blocking reader cannot be interrupted on every platform.
    // Do not wait for that thread after a fatal framing/output error.
    runtime.shutdown_background();
    status
}
