# Moly Client SDK

Experimental Rust SDK for connecting to an existing Moly Server. The CLI uses this
same library. Available as a workspace/path dependency; not published or a stable
Rust API yet. Wire compatibility is a separate contract.

The public surface is `Client`, `Events`, `Tool`, `Error`, and the `protocol` schema
re-export. JSONL framing, correlation, IPC types, and dispatch tasks stay private.
The SDK never imports Server code, launches processes, discovers configuration,
connects directly to Providers, installs tracing subscribers, or owns authoritative state.

## Connect and use a session

The caller supplies a Tokio runtime with I/O and timers. `Client::connect` takes a
Unix filesystem socket path or Windows local named-pipe name without the
`\\.\pipe\` prefix. `Client::from_stream` accepts an owned, full-duplex Tokio byte
stream and performs the same Server role/version handshake.

```no_run
use moly_client::{Client, protocol::EventKind};
use std::time::Duration;

# async fn example(endpoint: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
let (client, mut events) =
    tokio::time::timeout(Duration::from_secs(5), Client::connect(endpoint)).await??;
// The Server must already have a resolved configuration before starting a run.
let session = client.create_session().await?;
client.subscribe(session, 0).await?;
let run = client.start_run(session, "Summarize this workspace".into()).await?;
loop {
    let event = events.recv().await.ok_or("Server disconnected")?;
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
client.close(); // Disconnects all clones; does not stop the Server.
# Ok(())
# }
```

`config`, `validate_config`, `apply_config`, and `put_secret` accept explicit data;
there is no implicit environment or file lookup. Use `secret_ref` for a key supplied
through `put_secret`; never embed credentials in configuration options or launch
arguments/environment. Client–Server protocol v2 requires
`provider: {command: {executable, args, env}, options}` in `ResolvedConfig`.
The application resolves that command; only the Server spawns it. Provider-specific
options are opaque to Core. `apply_config` uses the observed revision for CAS.
`validate_config` invokes the selected Provider's validation operation but does not
change the revision, send credentials, or invoke a model.
Use `cancel_run(session, run)` for semantic cancellation, not a dropped RPC future.
`request::<T>` is a typed escape hatch for protocol extensions.

```no_run
use moly_client::{Client, Error, protocol::ResolvedConfig};

# async fn configure(client: &Client, config: ResolvedConfig) -> Result<(), Error> {
let snapshot = client.config().await?;
client.validate_config(&config).await?;
match client.apply_config(snapshot.revision, config).await {
    Err(Error::Remote(error)) if error.code == "revision_conflict" => {
        // Re-read and reconcile with the other Client; do not overwrite blindly.
        return Err(Error::Remote(error));
    }
    result => { result?; }
}
# Ok(())
# }
```

## Hosted tools

Use `Tool::new` with a `protocol::ToolDefinition` and an async callback returning
`Result<serde_json::Value, protocol::ProtocolError>`. Register with
`client.register_tools(session, tools)` before a run; an empty list unregisters.
The SDK returns results with the Server-issued lease automatically. Callbacks may
make ordinary requests through the same Client without blocking correlation.

```no_run
use moly_client::{Client, Error, Tool, protocol::{SessionId, ToolDefinition}};
use serde_json::json;

# async fn register(client: &Client, session: SessionId) -> Result<(), Error> {
let echo = Tool::new(
    ToolDefinition {
        name: "echo".into(),
        description: "Return the supplied arguments".into(),
        input_schema: json!({"type": "object"}),
    },
    |arguments| async move { Ok(arguments) },
);
client.register_tools(session, vec![echo]).await?;
# Ok(())
# }
```

Callbacks should not block the Tokio runtime. Prefer weak captures when a callback
references its own Client; strong captures form an ownership cycle that requires
explicit `Client::close`. Connection closure aborts callback futures but cannot
undo side effects. Server run cancellation fences late results and does not
necessarily abort a remote callback.

## Lifetime, replay, and errors

- Clones share one connection; dropping the last clone closes it. Server sessions
  and runs outlive that connection. `closed().await` observes closure, not completion
  of task destruction or Server cleanup.
- Keep and drain `Events` while subscribed. The SDK buffers at most 2,048 events;
  overflow or delivery to a dropped receiver closes the connection. Buffered events
  can still be drained after closure. There are at most 128 outstanding RPCs and
  128 active reverse callbacks.
- Record sequence numbers per session. Reconnect explicitly, then `subscribe` with
  the last observed sequence. Replays can redeliver events; an expired cursor is a
  structured `replay_unavailable` error. Events can arrive before the RPC response.
  `unsubscribe` stops future delivery, not events already buffered or in transit.
- There are no implicit deadlines, automatic retries, or reconnects. Wrap calls in
  `tokio::time::timeout` as needed. Canceling connection establishment cleans up its
  tasks; canceling an ordinary request releases its correlation slot but leaves
  its Server-side outcome uncertain. Never blindly retry side effects. Canceling
  tool registration after handlers are installed closes the connection and clears
  callbacks because the committed registry is uncertain; waiting for the registration
  lock can be canceled without affecting the connection.
- `Error` is non-exhaustive. Match `Error::Remote` by protocol code. I/O, closed
  connections, request capacity, framing, typed JSON, and incompatible handshakes
  have separate variants. The SDK does not log credentials, prompts, or tool data.
- Local peers are trusted. Use private Unix endpoint directories and review Windows
  pipe ACLs before crossing trust boundaries. Arbitrary streams require caller-owned
  authentication and transport security.

`examples/run.rs` is a small standalone SDK consumer for a configured Server:

```sh
cargo run --locked -p moly-client --example run -- <endpoint> <message>
```

It does not spawn or configure the Server. Build all workspace binaries before
running subprocess conformance, or set test-only `MOLY_TEST_SERVER_BIN` and
`MOLY_TEST_PROVIDER_BIN` overrides. Python 3 is required for the independent
Provider fixture. No live service is used by ordinary tests.
