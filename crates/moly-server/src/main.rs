//! Standalone Server: private Core, provider, executor, and transport modules.
mod core;
mod executors;
mod model_provider;
mod secrets;
#[cfg(test)]
use moly_protocol as protocol;
#[cfg(test)]
mod tests;
mod transport;

use std::io::Write;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 || args[0] != "--endpoint" {
        return Err("usage: moly-server --endpoint <local endpoint in a private directory>".into());
    }
    let executor = executors::LocalExecutor::new();
    let server = core::Server::with_local_executor(Some(executor))?;
    let listener = transport::local::Listener::bind(&args[1])?;
    // One readiness record, not a second agent-state protocol or STDIO RPC mode.
    println!(
        "{}",
        serde_json::json!({"ready":true,"server_id":server.id(),"endpoint":args[1]})
    );
    std::io::stdout().flush()?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    transport::serve(server, listener, shutdown).await?;
    Ok(())
}
