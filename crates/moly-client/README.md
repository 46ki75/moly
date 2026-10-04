# Moly Agent Client SDK

Experimental Rust SDK for connecting to an existing Agent Server. Agent Server-backed
CLI operations use this library. Agent Server is the component name; `moly-server`
and existing Rust/wire identifiers remain unchanged for compatibility. Older Server
terminology aliases Agent Server. Direct Provider access uses the separate
[`moly-provider-client`](../moly-provider-client/README.md) MPP SDK instead. Available as a workspace/path dependency; not published or a stable
Rust API yet. Wire compatibility is a separate contract.

Contributors must read [CONTRIBUTING.md](../../CONTRIBUTING.md) before making changes.

The public surface is `Client`, `Events`, `Tool`, `Interaction`, `Error`, and the
`protocol` schema re-export. JSONL framing, correlation, IPC types, and dispatch tasks stay private.
The SDK never imports Agent Server code, launches processes, discovers configuration,
connects directly to Providers, installs tracing subscribers, or owns authoritative
state or an agentic loop. The Agent Server owns that loop. Session Store access
remains Agent Server-only; storage integration and CLI save/resume are not implemented.

## Connect and use a session

The caller supplies a Tokio runtime with I/O and timers. `Client::connect` takes a
Unix filesystem socket path or Windows local named-pipe name without the
`\\.\pipe\` prefix. `Client::from_stream` accepts an owned, full-duplex Tokio byte
stream and performs the same Agent Server role/version handshake (wire role `server`).

```no_run
use moly_client::{Client, protocol::EventKind};
use std::time::Duration;

# async fn example(endpoint: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
let (client, mut events) =
    tokio::time::timeout(Duration::from_secs(5), Client::connect(endpoint)).await??;
// The Agent Server must already have a resolved configuration before starting a run.
let session = client.create_session().await?;
client.subscribe(session, 0).await?;
let run = client.start_run(session, "Summarize this workspace".into()).await?;
loop {
    let event = events.recv().await.ok_or("Agent Server disconnected")?;
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
client.close(); // Disconnects all clones; does not stop the Agent Server.
# Ok(())
# }
```

`config`, `validate_config`, `apply_config`, and `put_secret` accept explicit data;
there is no implicit environment or file lookup. Use `secret_ref` for a key supplied
through `put_secret`; never embed credentials in configuration options or launch
arguments/environment. Client–Agent Server protocol v3 requires
`provider: {command: {executable, args, env}, options}` in `ResolvedConfig`.
The application resolves that command; the Agent Server owns all Provider processes for
operations routed through this SDK. Provider-specific
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

## Explicit authentication and interactions

`client.authenticate(AuthCommand, Option<Interaction>)` sends `provider.auth` and
returns `AuthStatus`. Choose `Login`, `Status`, or `Logout`, a fresh `AuthAttemptId`,
and the observed config revision. The selected config needs a `secret_ref`, even
when login starts with an empty credential slot. Status is a local credential
inspection, not proof of model access. Validation never authenticates.

`Interaction::new` accepts an async callback returning `Result<InteractionOutcome,
ProtocolError>`. It is installed **before** the auth request and invoked only for
`interaction.request` matching that live login attempt on this connection. The SDK
binds the response identity automatically. `Opened` means presented, not successful
OAuth. With `None` (the headless default), or for unknown/completed/non-login attempts,
the SDK returns `interaction_unavailable`; it never prompts or opens a browser.

```no_run
use moly_client::{Client, Error, Interaction, protocol::{AuthAttemptId, auth::*}};

# async fn login(client: &Client) -> Result<(), Error> {
let snapshot = client.config().await?;
let attempt_id = AuthAttemptId::new();
let interaction = Interaction::new(|request| async move {
    // Application policy: validate HTTPS and present through your own UI.
    // Do not log the URL, evaluate a shell command, or treat presentation as login.
    let _ = request;
    Ok(InteractionOutcome::Declined) // Replace with your UI's actual outcome.
});
let login = client.authenticate(
    AuthCommand { attempt_id, operation: AuthOperation::Login, config_revision: snapshot.revision },
    Some(interaction),
);
tokio::pin!(login);
tokio::select! {
    result = &mut login => { result?; }
    _ = tokio::signal::ctrl_c() => {
        client.cancel_auth(attempt_id).await?;
        // The original terminal reply decides whether completion won the race.
        // Cancellation normally returns Error::Remote with code auth_cancelled.
        login.await?;
    }
}
# Ok(())
# }
```

Callbacks may issue nested commands through the same Client. Registrations are weak
and per-attempt, not persistent: captured Client clones do not create a registry
ownership cycle. Completion, failure, dropped auth futures, and connection closure
remove presentation authority and abort pending callback futures. `cancel_auth`
sends `auth.cancel` and withdraws presentation when that RPC returns; keep awaiting
the original auth operation for its terminal response. Dropping the auth future
sends best-effort cancellation but does not confirm the remote outcome. If that
cleanup cancellation cannot be acknowledged, the SDK closes the connection so the
Agent Server can abort its owned auth operation.

Do not wrap human login in a short command timeout. The Agent Server uses a separate
300-second login deadline (30 seconds for status/logout). A committed credential
replacement can survive cancellation or a lost result. Inspect status rather than
blindly retrying login. `AuthStatus.registration` is opaque **nonsecret** metadata;
applications may explicitly persist it, but must not persist credentials or adopt
registration from unrelated configured Providers. All configuration, file ownership,
and browser/UI policy remain outside this SDK. See [authentication](../../docs/authentication.md).

### CLI-owned OAuth pilot policy

This section describes Agent Server-backed mode. [`moly --direct`](../../README.md)
uses the MPP SDK instead and keeps credentials in the CLI process, not an Agent Server.

The `moly` executable offers `/login`, `/auth` (or `/auth status`), and `/logout`.
These connect/configure lazily, even as the first operation, without creating a
conversation. `/help`, `/new`, blank input, and exit before a backend operation remain
local. Ctrl-C cancels auth on the same connection and waits for its terminal reply;
EOF or `/quit` during auth cancels and exits. Other auth-time input is ignored.

For an unconfigured Agent Server, explicitly set:

```sh
export MOLY_PROVIDER=openai-codex
export MOLY_MODEL='<explicit available model>'
export MOLY_AUTH_STATE_FILE='/absolute/private-directory/auth.json'
moly
```

The directory must already exist and be trusted/private. Relative state paths resolve
against the CLI's working directory; there is no implicit home/config location.
The profile selects the sibling `moly-provider-openai-codex` binary (or an explicit
`MOLY_PROVIDER_EXECUTABLE`) with options `{model, host_id, registration?}`. It requires
a nonblank model, reserves a credential reference even without `MOLY_API_KEY`, and
never reads/sends that API key or uses `MOLY_MODEL_ENDPOINT`. There is no API-key fallback.
The CLI validates and explicitly displays an HTTPS authorization URL for the user
to open; it does not launch a shell or automatically start a browser. Token-bearing
URLs, including `id_token_hint`, are refused.

The state file is **CLI-owned**, not SDK/Agent Server-owned: `{host_id, registration?}` only.
On Unix it uses owner-only files, a sibling `.lock` for cooperating CLI writers,
and atomic synced replacement; a busy lock requires explicit retry. Symlinks,
insecure permissions, credential-like fields,
oversized/deep registration, and changed local state are rejected. Registration stays
opaque, with defensive nonsecret constraints and host binding when supplied; these
checks are not a general secret detector. Secure state-file ACLs are not implemented
on Windows, so initializing this managed profile currently requires Unix. Host identity and
registration survive Agent Server restart; tokens do not, so login is required again.

An already configured Agent Server always wins over ambient CLI configuration. Login and
status can persist returned registration only when `MOLY_PROVIDER=openai-codex` is
explicitly selected and the managed local state, executable/arguments/environment,
model, host ID, credential reference, and any existing registration match the
configured profile. A missing local registration can be recovered, but conflicting
nonempty registrations require explicit reconciliation rather than an account switch.
Attaching to another Provider does not adopt its registration. A config update adds registration
only to the unchanged auth snapshot via CAS; another Client's configuration is never
blindly overwritten. If the config changed, registration remains saved locally and
the CLI reports the conflict. `/auth` can recover registration after a lost login
result while the Agent Server's in-memory credential still exists.

## Hosted tools

Use `Tool::new` with a `protocol::ToolDefinition` and an async callback returning
`Result<serde_json::Value, protocol::ProtocolError>`. Register with
`client.register_tools(session, tools)` before a run; an empty list unregisters.
The SDK returns results with the Agent Server-issued lease automatically. Callbacks may
make ordinary requests through the same Client without blocking correlation.

Return known execution failures as `Ok(json!({"error": {"code": ..., "message": ...}}))`
so the model receives a correlated result and can recover. Sanitize descriptions at
source: Core keeps output opaque and does not redact it. Reserve `Err(ProtocolError)`
for RPC/execution faults that must terminate the run; the SDK does not reclassify them.
`ToolCompleted` means an outcome was committed, not that the operation succeeded.

```no_run
use moly_client::{Client, Error, Tool, protocol::{SessionId, ToolDefinition}};
use serde_json::json;

# async fn register(client: &Client, session: SessionId) -> Result<(), Error> {
let echo = Tool::new(
    ToolDefinition {
        name: "echo".into(),
        description: "Return the supplied text or a sanitized argument error".into(),
        input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
    },
    |arguments| async move {
        let Some(text) = arguments.get("text").and_then(serde_json::Value::as_str) else {
            return Ok(json!({"error": {"code": "invalid_arguments", "message": "Expected a text string"}}));
        };
        Ok(json!({"text": text}))
    },
);
client.register_tools(session, vec![echo]).await?;
# Ok(())
# }
```

Callbacks should not block the Tokio runtime. Prefer weak captures when a callback
references its own Client; strong captures form an ownership cycle that requires
explicit `Client::close`. Connection closure aborts callback futures but cannot
undo side effects. Agent Server run cancellation fences late results and does not
necessarily abort a remote callback.

## Lifetime, replay, and errors

- Clones share one connection; dropping the last clone closes it. Agent Server sessions
  and runs outlive that connection. `closed().await` observes closure, not completion
  of task destruction or Agent Server cleanup.
- Keep and drain `Events` while subscribed. The SDK buffers at most 2,048 events;
  overflow or delivery to a dropped receiver closes the connection. Buffered events
  can still be drained after closure. There are at most 128 outstanding RPCs and
  128 active reverse callbacks.
- Record sequence numbers per session. Reconnect explicitly, then `subscribe` with
  the last observed sequence. Replays can redeliver events; an expired cursor is a
  structured `replay_unavailable` error. Events can arrive before the RPC response.
  `unsubscribe` stops future delivery, not events already buffered or in transit.
- There are no implicit auth/model deadlines, automatic retries, or reconnects.
  Wrap operations in `tokio::time::timeout` as needed; dropped auth futures send a
  best-effort cancel RPC with a bounded cleanup deadline. Canceling connection establishment cleans up its
  tasks; canceling an ordinary request releases its correlation slot but leaves
  its Agent Server-side outcome uncertain. Never blindly retry side effects. Canceling
  tool registration after handlers are installed closes the connection and clears
  callbacks because the committed registry is uncertain; waiting for the registration
  lock can be canceled without affecting the connection.
- `Error` is non-exhaustive. Match `Error::Remote` by protocol code. I/O, closed
  connections, request capacity, framing, typed JSON, and incompatible handshakes
  have separate variants. The SDK does not log credentials, prompts, or tool data.
- Local peers are trusted. Use private Unix endpoint directories and review Windows
  pipe ACLs before crossing trust boundaries. Arbitrary streams require caller-owned
  authentication and transport security.

`examples/run.rs` is a small standalone SDK consumer for a configured Agent Server:

```sh
cargo run --locked -p moly-client --example run -- <endpoint> <message>
```

It does not spawn or configure the Agent Server. Build all workspace binaries before
running subprocess conformance, or set test-only `MOLY_TEST_SERVER_BIN` and
`MOLY_TEST_PROVIDER_BIN` overrides. Python 3 is required for the independent
Provider fixture. No live service is used by ordinary tests.
