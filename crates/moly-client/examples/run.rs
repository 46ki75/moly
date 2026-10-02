//! Standalone SDK consumer; requires an existing, configured Server.
use moly_client::{Client, protocol::EventKind};
use std::time::Duration;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [endpoint, message] = args.as_slice() else {
        return Err("usage: run <local Server endpoint> <message>".into());
    };
    let (client, mut events) =
        tokio::time::timeout(Duration::from_secs(5), Client::connect(endpoint)).await??;
    if client.config().await?.config.is_none() {
        return Err(
            "Server is not configured; apply a resolved config through a Client first".into(),
        );
    }
    let session = client.create_session().await?;
    client.subscribe(session, 0).await?;
    let run = client.start_run(session, message.clone()).await?;
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                client.cancel_run(session, run).await?;
            }
            event = events.recv() => {
                let event = event.ok_or("Server disconnected; run outcome is unknown")?;
                if event.session_id != session {
                    continue;
                }
                match event.kind {
                    EventKind::AssistantMessage { run_id, text } if run_id == run => println!("{text}"),
                    EventKind::RunCompleted { run_id } if run_id == run => break,
                    EventKind::RunFailed { run_id, error } if run_id == run => return Err(error.into()),
                    EventKind::RunCancelled { run_id } if run_id == run => return Err("run cancelled".into()),
                    _ => {}
                }
            }
        }
    }
    client.close();
    Ok(())
}
