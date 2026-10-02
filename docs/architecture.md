# Architecture generation 0

Status: experimental, in-memory, single-machine runtime with replaceable Provider
processes. Session Store protocols and persistence are not implemented yet. The
repository started empty; there was no prior build or behavioral baseline. No implementation
has been archived yet.

## Dependency boundaries

| Crate | Responsibility | Surface / stability | Allowed project dependencies |
| --- | --- | --- | --- |
| `moly-protocol` | IDs, envelopes, Component payloads, events, resolved config | Protocol / envelope v1, Server v2, Provider v1 | None |
| `moly` | User-facing CLI executable; lazy process spawning and config resolution | Product / experimental, unpublished | Client SDK only |
| `moly-client` | Public headless Client API; private transport and callback dispatcher | SDK / experimental, unpublished | Protocol only |
| `moly-server` | Server executable; Core, Provider process supervision, executors, and server-side transport | Product / experimental, unpublished | Protocol only |
| `moly-provider-openai` | Standalone STDIO Model Provider; HTTP encoding/authentication | Protocol implementation / experimental, unpublished | Protocol only |

All three executable crates are binary-only. None depends on another executable
crate, including for tests. Client/Server and Server/Provider communication crosses
process protocol boundaries. The CLI uses `moly-client`; the Server never imports
that SDK. `moly-protocol` is the only project crate shared across these role boundaries
and has no Tokio, connection management, provider implementation, or transport dependencies.

```text
crates/
├── moly/src/
│   ├── main.rs        argv and lazy runtime/tracing composition
│   ├── repl.rs        line input, local commands, and SDK event rendering
│   └── backend.rs     CLI-owned config and unmanaged executable spawning
├── moly-client/src/
│   ├── lib.rs         public Client, Events, Tool, Error, and protocol facade
│   ├── client.rs      semantic Client and callback dispatcher
│   └── transport/     private client-side IPC, JSONL, duplex RPC
├── moly-server/src/
│   ├── main.rs        Server composition root
│   ├── core.rs        authoritative live state machine
│   ├── model_provider.rs  Server-owned child processes and Provider protocol client
│   ├── secrets.rs     generic memory-only credential storage
│   ├── executors.rs   physical local side effects
│   └── transport/     listener, protocol adapter, JSONL, duplex RPC
├── moly-provider-openai/src/  private STDIO service and concrete HTTP adapter
└── moly-protocol/src/        Client/Server and Model Provider schemas
```

The SDK and Server own separate transport implementations. Initial duplication is
deliberate: connection machinery does not belong in the schema crate, and no
source-inclusion shortcut may couple the roles. Both implementations run the same
root protocol conformance suites. The bundled Provider has an independent bounded
STDIO implementation. Core and executor boundaries remain Server-internal modules;
concrete provider code is neither linked into the Server nor a Rust library API.

The [Client SDK](../crates/moly-client/README.md) provides connections, typed commands,
events, and hosted callbacks, not application policy. It uses the caller's Tokio
runtime and has no process spawning, config discovery, provider, automatic retry,
or reconnection logic. The CLI and standalone SDK example use this same library.

```text
moly executable
    │ moly-client SDK → private client-side transport
    │ local byte stream (Unix socket / Windows named pipe)
    ▼
moly-server executable: server-side transport / connection adapter
    │ ordinary command / connection-scoped effect channels
    ▼
Server router → session mailbox → sequential live state machine
                                   │
                                   ├ model step → Provider JSONL → spawned Provider → HTTP
                                   └ hosted tool → local executor / Client reverse RPC
```

One listener accepts multiple direct Clients. Server, session, run, model call,
executor, connection, and ToolRun identities are separately typed UUIDs, not PIDs
or endpoint paths. A Server ID identifies this live incarnation; crash restoration
and identity across restoration are not implemented.

## Authority and execution

Each session actor alone commits its canonical sequence. Different sessions
progress independently. Provider/executor tasks return results through that
mailbox; they do not mutate authoritative state. One active run per session is
supported. Completion, failure, and cancellation are terminal. A cancelled run's
late completions cannot mutate a replacement run.

A run pins resolved config at acceptance. Server config updates use a global
compare-and-swap revision, separate from per-session event sequences. The Server
never searches for config files or expands environment variables. The CLI resolves
environment overrides after the first user input requiring a backend. Secrets are
sent separately to an in-memory generic store and referenced by key in config.
This prototype has no provider auth interaction, refresh, or durable secret store.

Every inference step originates in the Server actor, with SessionId, RunId,
ModelCallId, and call kind. The [Model Provider protocol](model-provider-protocol.md)
uses normalized user/assistant/tool-result context and opaque replay metadata.
Only the selected Provider encodes upstream requests and interprets authentication.
The bundled process implements nonstreaming OpenAI-compatible Chat Completions,
including an explicit OpenCode Go profile, and returns `Completed` or `AwaitHostTools`.
The Go profile derives its session header from the Server-issued SessionId, independent
of Provider child lifetime. Reasoning text stays in opaque replay metadata, not
assistant output. Core never constructs Chat Completions JSON or Go headers.
Unsupported outcomes fail explicitly; native features and opaque continuations are
not fabricated. Generic affinity/cache controls, streaming, and usage accounting
remain unimplemented.

The Client supplies the Provider executable, literal arguments, explicit environment,
and opaque options. Validation uses that implementation without model access or
credentials, and configuration is committed only after successful validation and CAS.
The Server lazily spawns one process per validation/model step over STDIO and terminates
it afterward. Handshake/validation deadlines are three seconds each; a model step has
a 65-second deadline. No process reuse, shell/PATH lookup, service attachment, or
transparent retries occur. Cancellation drops and kills the owned child; Provider
errors and stderr are never forwarded verbatim. This is a hosting policy, not a
requirement that every implementation have a separate package or fixed process count.
Third-party executables are trusted code, not an OS sandbox.

The standalone binary explicitly composes `read_file` as a separate executor.
The same Core can run without a local executor and accept Client-hosted tools only.
`read_file` is intentionally not a shell tool. Canonical path checks confine ordinary
reads to the workspace, but are **not an OS sandbox**: concurrent filesystem mutation
can race checks. Use only trusted local workspaces. Client tools use the same logical
lease/result contract. A ToolRun has an independent ID, chosen executor, and
execution generation. V0 issues generation 1 only and rejects mismatched leases;
it does not retry or reassign side effects. Cancellation does not undo side effects
already performed, and remote callback cancellation is best-effort (connection
closure aborts callbacks; run cancellation rejects their late results).

Conversation history is extended with the accepted user message. Tool-call and
assistant working context is promoted on successful completion only; cancellation
and failure discard that run's partial model/tool context. Canonical events still
record its observable committed transitions.

## CLI startup and process lifetime

```text
minimal argv → flush line prompt → read input / handle local commands
    → first message → initialize runtime/tracing → connect or spawn Server
    → use existing config, or resolve/apply it in the CLI → session → run
```

V0 is a single-line REPL, not a raw-terminal/full-screen TUI. Help, blank lines,
unknown slash commands, `/new`, and exit before the first message do not create a
runtime, connect, scan a project, or initialize a provider. Messages reuse one
session until `/new`; old sessions remain Server-owned. Input during a run waits
until it terminates. Ctrl-C cancels that run, or exits when idle.

A bounded input channel fed by a detached OS thread keeps blocked stdin reads out
of Tokio's shutdown path. The idle loop drains events and notices connection loss
without another keystroke. Configuration and run errors return to the prompt;
connection loss and 10-second SDK command deadlines exit without automatic retry.
Inference has no CLI-wide deadline. There is no line editor, persistent input
history, completion, or streaming rendering. Backend startup waits for a readiness
record, not PID/process scanning or a polling sleep. The Server is explicitly
unmanaged and survives CLI exit. The printed endpoint allows another protocol Client to attach directly. V0 has no
idle shutdown, session browser, or daemon discovery. Explicitly stop the process when
finished; its printed PID is diagnostic only. The Server does not receive a hidden
shutdown command on disconnect. Spawned Servers use a separate process group so
terminal Ctrl-C reaches the CLI's cancellation handler, not the Server.

## Deliberate limits and pressure points

- No Daemon yet: no Server adoption, Daemon supervision/restart, or scheduler.
  Checkpoints, persistence, durable ACKs, and multi-agent orchestration are also absent. Logical identities and
  direct multi-client connections leave room to add it as an ordinary SDK Client.
- No remote access policy, TLS gateway, TCP listener, WebSocket, or Client–Server
  STDIO RPC (Provider STDIO is supported). Local peers are trusted and equally authoritative. Unix endpoints belong in an
  owner-only directory. Windows pipe ACL behavior needs platform/security review
  before use across trust boundaries.
- Event history is a bounded in-memory suffix, not durability. Disconnection never
  deletes a session or cancels a run. Slow consumers close instead of silently
  dropping events; reconnect explicitly with the last observed sequence.
- Sessions and conversation context are memory-resident and currently have no
  quota, deletion, compaction, or expiration. This is a local experiment, not a
  hardened multi-tenant service.
- No automatic reconnect, request deduplication, or exactly-once command delivery.
  A lost response leaves command outcome uncertain: inspect/replay, do not blindly
  retry side effects. Request cancellation is not semantic run cancellation.
- Tool registrations are connection-scoped and session-specific. No placement
  policy, tool input-schema validation engine, tool-output streaming, retries,
  process trees, or persistent executors. An executor disappearing during work
  fails the run rather than migrating its side effect.
- Config updates have CAS but no `config.changed` subscription yet. Secret updates
  are separate and unversioned; a config revision does not freeze secret contents.
- Provider protocol v1 supports cross-language extensions but is experimental, not
  a stable API. No persistent Provider workers, reverse host-service RPC, dynamic
  Rust ABI, WASM, cluster, or consensus abstraction.

These are scope limits, not claims that the architecture has already failed in
production. Record measured pressure before adding machinery.

## Lessons verified during implementation

- The initial five-crate packaging obscured application ownership: the CLI package
  imported Server internals and housed both binaries. Role consolidation made the
  process boundary structural. The subsequently requested Client SDK extracts only
  client-side code; it does not reintroduce a shared runtime. A Cargo-metadata
  regression now guards five crates, and SDK fixtures spawn the actual Server
  instead of linking it. Provider extraction removes HTTP from Server dependencies.
  Server protocol v2 intentionally changes resolved configuration; envelope v1 and
  the original trace fixture remain preserved. These packaging changes do not
  require a new archive generation.
- Logical lifetime separation is insufficient if the spawned Server inherits the
  CLI's foreground process group. A terminal-group SIGINT originally killed both
  processes. Separate process groups fixed it; the Unix product smoke now checks
  cancellation followed by direct reattachment after CLI exit. Windows uses
  [`CREATE_NEW_PROCESS_GROUP`](https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags);
  its console behavior still needs native runtime verification.
- Cancelling a handshake future originally leaked its detached dispatcher and
  stream. An establishment guard now closes both; a regression test aborts the
  handshake after observing its request and verifies EOF.
- Slow-consumer closure must interrupt an already-blocked writer enqueue, not
  just the outer connection loop. A regression saturates all three output layers
  while another Client continues mutating the session, then verifies closure.
- Closing a Client and rolling back a concurrent tool registration must share
  lifecycle state. Otherwise rollback can resurrect callbacks and ownership cycles
  after explicit close. The close/rollback regression checks resource release.
- Canceling an in-flight tool registration originally retained newly installed
  handlers and any captured Client cycles. The SDK now closes that connection on
  interrupted registration: it cannot know which registry the Server committed.
  A gated regression verifies closure and callback release after future cancellation.
- Validation must agree across boundaries: the Core originally accepted a
  whitespace-only model that the provider rejected. A regression test now requires
  rejection before any configuration revision advances.

## Rewrite protocol

When a real architectural limit appears: record the failure, extract its invariant,
tag the revision, snapshot the old workspace into `archive/vN`, and start small
again. Each immutable archive owns its Cargo manifest/lock, optional toolchain, and
a README covering Model, What worked, Architectural limit, New invariant, and
Superseded by. Root Cargo must exclude it. Never move root `conformance/` assets
into an archive: those semantics and traces outlive implementations.
