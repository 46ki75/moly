# Architecture generation 0

Status: experimental, in-memory, single-machine runtime with replaceable Provider
processes. The Moly Provider Protocol (MPP) host boundary supports the Agent Server and
explicit direct CLI model-only chat. Session Store protocols and conversation
persistence are not implemented yet. The repository started empty; no implementation
has been archived. See [verification results and limits](verification.md).

Agent Server is the formal component name: it owns the agentic loop, coordinating
model calls, hosted tools, and continuation. Older **Server** terminology aliases
Agent Server. The `moly-server` executable/crate, Rust symbols such as `ServerId`
and `SERVER_VERSION`, wire role `server`, method strings, schema IDs/filenames, and
protocol versions remain unchanged for compatibility.

## Conversation history contract

[Conversation history v1](conversation-history.md) is a separate, independently
versioned data contract in `moly-protocol::history`, not MPP or an Agent Server
runtime checkpoint. Pure JSONL codecs/validation preserve session IDs, typed entry
ancestry, selected heads, and opaque Provider replay/continuation data. They do not
activate sessions, reconstruct model context, persist credentials, or execute work.
There is still no Session Store, runtime save/resume, or direct-mode persistence.

The snapshot codec bounds records to 1 MiB, snapshots to 64 MiB, entries to 100,000,
and JSON-valued payloads to 64 nested edges. Unknown semantic records, malformed
references, duplicate JSON keys, and lossy integer conversions fail explicitly.
These are history limits, independent of MPP framing and Agent Server event replay.

## Dependency boundaries

| Crate | Responsibility | Surface / stability | Allowed project dependencies |
| --- | --- | --- | --- |
| `moly-protocol` | IDs, Component payloads, history DTOs and pure validation | Envelope v1, Agent Server v3, MPP v2, history v1 (independent) | None |
| `moly` | CLI executable; lazy backend selection, config resolution, UI, local direct context | Product / experimental, unpublished | `moly-client`, `moly-provider-client` |
| `moly-client` | Agent Client SDK; public headless Agent Server API, private transport and callback dispatcher | SDK / experimental, unpublished | `moly-protocol` only |
| `moly-provider-client` | Common MPP host SDK; explicit Provider child supervision and private STDIO transport | SDK / experimental, unpublished | `moly-protocol` only |
| `moly-server` | Agent Server executable; agentic-loop Core, credential authority, Provider host policy, executors, and Agent Server-side transport | Product / experimental, unpublished | `moly-protocol`, `moly-provider-client` |
| `moly-provider-openai` | Standalone STDIO Model Provider; Chat Completions HTTP | Protocol implementation / experimental, unpublished | Protocol only |
| `moly-provider-openai-codex` | Standalone STDIO Provider; ChatGPT OAuth and Responses SSE | Protocol implementation / experimental, unpublished | Protocol only |

All four executable crates are binary-only. None depends on another executable
implementation, including for tests or by source inclusion. Client/Agent Server and
host/Provider communication cross process protocol boundaries. The CLI uses
`moly-client` for Agent Server access and `moly-provider-client` for explicit direct MPP
access; the Agent Server uses the MPP SDK, never the Agent Client SDK. Both SDKs depend
only on `moly-protocol` among project crates. The schema crate has no Tokio,
connection management, Provider implementation, or transport dependencies.
Bundled Providers depend only on protocol among project crates; upstream HTTP/OAuth
belongs in those executables, not either SDK or the Agent Server.

```text
crates/
├── moly/src/
│   ├── main.rs        argv and lazy runtime/tracing composition
│   ├── repl.rs        line input, local commands, and SDK event rendering
│   ├── backend.rs     shared CLI config resolution and Agent Server startup policy
│   └── direct.rs      direct MPP chat/auth host; local history and credential slot
├── moly-client/src/
│   ├── lib.rs         public Client, Events, Tool, Error, and protocol facade
│   ├── client.rs      semantic Client and callback dispatcher
│   └── transport/     private client-side IPC, JSONL, duplex RPC
├── moly-provider-client/src/  public MPP operations; private framing/supervision
├── moly-server/src/
│   ├── main.rs        Agent Server composition root
│   ├── core.rs        authoritative live state machine
│   ├── model_provider.rs  MPP SDK adapter; binds UI, Core retains leases/deadlines
│   ├── secrets.rs     generic memory-only credential storage
│   ├── executors.rs   physical local side effects
│   └── transport/     listener, protocol adapter, JSONL, duplex RPC
├── moly-provider-openai/src/  private STDIO service and Chat Completions adapter
├── moly-provider-openai-codex/src/  private OAuth and Responses adapter
└── moly-protocol/src/        Component schemas; independent history DTOs/validation
```

The Agent Client SDK and Agent Server retain separate local-IPC transports. The common
MPP SDK owns a third, private host-side STDIO transport shared by the Agent Server and
direct CLI. Each Provider retains its independent bounded STDIO implementation.
Transport does not belong in the schema crate; sharing host machinery does not
link executable implementations or transfer authority. Core and executors remain
Agent Server-internal; concrete Provider code is not a Rust library API.

The [Agent Client SDK](../crates/moly-client/README.md), `moly-client`, provides
Agent Server connections, typed commands, events, and hosted callbacks. It uses the
caller's Tokio runtime and has no process spawning, config discovery, Provider
hosting, automatic retry, or reconnection logic. Normal CLI mode and the standalone
Agent Server-client example use this library.

The [MPP host SDK](../crates/moly-provider-client/README.md), `moly-provider-client`,
exposes stateless `ProviderClient`
(`Default`/`Clone`/`Copy`) operations `validate`, `authenticate`, and `step`, plus
an `Interaction` callback for explicit authentication. It re-exports `moly-protocol`
as `protocol`. Each operation launches one initialized child from a resolved
`ProviderConfig`; options and credentials come from that config and the borrowed
credential slot, not stale copies in request fields. A step without a credential
scope sends no credential. Status may read but not replace; login/logout/model
refresh may replace only the selected slot. Committed replacement survives later
errors or cancellation. The host must serialize the slot for the entire operation;
the SDK has no secret store.

Interaction callbacks run inline and are dropped with cancellation, not retained
in a detached registry. The SDK supplies the matching attempt ID in its reverse
response. It owns framing, correlation, bounded cleanup, and sanitized typed
`ProtocolError` failures, not config discovery, UI, persistence, conversation policy,
or an agent/tool loop.

```text
moly executable
    │ moly-client SDK → private client-side transport
    │ local byte stream (Unix socket / Windows named pipe)
    ▼
moly-server executable: Agent Server-side transport / connection adapter
    │ ordinary command / connection-scoped effect channels
    ▼
Agent Server router → session mailbox → sequential live state machine
                                         │
                                         ├ model step → MPP SDK → Agent Server-owned Provider → HTTP
                                         └ hosted tool → local executor / Client reverse RPC

moly --direct executable: local context / credential slot / interaction UI
    │ moly-provider-client SDK → private MPP STDIO transport
    ▼
CLI-owned Provider executable → upstream HTTP/OAuth
    (no agentic loop, Agent Server, Session Store, tools, or session/replay authority)
```

One Agent Server listener accepts multiple directly connected Clients. Agent Server,
session, run, model call, executor, connection, and ToolRun identities are separately
typed UUIDs, not PIDs or endpoint paths. An Agent Server ID identifies this live incarnation; crash restoration
and identity across restoration are not implemented.

## Authority and execution

The Agent Server alone owns the agentic loop and is authoritative for agent runs
routed through it. Direct CLI hosting provides only model-chat/authentication, not
an agentic loop or access to Agent Server sessions, history, ToolRuns, or subscriptions.
Session Store access remains Agent Server-only under [the requirements](REQUIREMENTS.md);
Session Store integration, conversation persistence, and CLI save/resume are not implemented.

Each Agent Server session actor alone commits its canonical sequence. Different sessions
progress independently. Provider/executor tasks return results through that
mailbox; they do not mutate authoritative conversation state. One active run per session is
supported. Completion, failure, and cancellation are terminal. A cancelled run's
late completions cannot mutate a replacement run.

A run pins resolved config at acceptance. Agent Server config updates use a global
compare-and-swap revision, separate from per-session event sequences. The Agent Server
never searches for config files or expands environment variables. The CLI resolves
environment overrides after the first user input requiring a backend. Secrets are
sent separately to an in-memory generic store and referenced by key in config.
Explicit [authentication](authentication.md) routes Provider interactions
to the initiating Client. Providers own OAuth/refresh mechanics and replace opaque
records through the MPP SDK's scoped host service; Core does not interpret tokens.
The Agent Server holds the credential lease for the full SDK operation, including inline
interaction. No durable secret store or zeroizing vault is implemented; login is
required after Agent Server restart.

Every inference step in an Agent Server agent run originates in its actor, with SessionId,
RunId, ModelCallId, and call kind. The [Moly Provider Protocol (MPP)](model-provider-protocol.md)
uses normalized user/assistant/tool-result context and opaque replay metadata.
A direct CLI supplies local conversation and fresh turn/model identities instead;
these are MPP correlations, not Agent Server sessions or runs.
Only the selected Provider encodes upstream requests and interprets authentication.
The bundled `moly-provider-openai` implements nonstreaming OpenAI-compatible Chat Completions,
including an explicit OpenCode Go profile, and returns `Completed` or `AwaitHostTools`.
The Go profile derives its session header from the host-supplied SessionId, independent
of Provider child lifetime: Agent Server-issued in normal mode, CLI-local in direct mode.
Reasoning text stays in opaque replay metadata, not assistant output. Core never
constructs Chat Completions JSON or Go headers.
Unsupported outcomes fail explicitly; native features and opaque continuations are
not fabricated. The separate Codex Provider buffers Responses SSE into the same
outcomes while keeping OAuth outside Core. Generic affinity/cache controls,
Client-visible streaming, and usage accounting remain unimplemented.

The Client supplies the Provider executable, literal arguments, explicit environment,
and opaque options. Validation uses that implementation without model access or
credentials. In Agent Server mode, configuration is committed only after successful
validation and CAS; direct mode has no Agent Server config revision.
The host invokes the common MPP SDK to lazily spawn one process per validation,
authentication operation, or model step over STDIO and terminate it afterward.
For Agent Server-routed operations the Agent Server alone owns those children; direct CLI
operations own independent children. Handshake/validation deadlines are three
seconds each; a model step has a 65-second deadline. The Agent Server includes credential
contention in its total deadline. Login has 300 seconds; status/logout have 30
seconds. No process reuse, shell/PATH lookup, ambient environment inheritance,
service attachment, detached workers, or transparent retries occur. Dropping the
operation kills its owned child with bounded cleanup; Provider errors and stderr
are never forwarded verbatim. This is a hosting policy, not a
requirement that every implementation have a separate package or fixed process count.
Third-party executables are trusted code, not an OS sandbox.

The standalone Agent Server binary explicitly composes `read_file` as a separate executor.
The same Core can run without a local executor and accept Client-hosted tools only.
`read_file` is intentionally not a shell tool. Canonical path checks confine ordinary
reads to the workspace, but are **not an OS sandbox**: concurrent filesystem mutation
can race checks. Use only trusted local workspaces. Client tools use the same logical
lease/result contract. A ToolRun has an independent ID, chosen executor, and
execution generation. V0 issues generation 1 only and rejects mismatched leases;
it does not retry or reassign side effects. Cancellation does not undo side effects
already performed, and remote callback cancellation is best-effort (connection
closure aborts callbacks; run cancellation rejects their late results).

Known `read_file` execution failures are typed in the executor and returned as
sanitized `output.error` data with the original lease. Core commits the result,
retains the Provider call ID, finishes the accepted tool batch, and resumes inference
within the existing 16-step limit. `ToolCompleted` denotes a committed outcome,
not successful file access. Invalid workspace configuration, authority violations,
transport faults, and reverse RPC errors still terminate the run. Client-hosted
tools use the same output convention explicitly; Core does not classify arbitrary
RPC errors by their codes. See [tool outcomes](protocol.md#tool-execution-outcomes).

Agent Server conversation history is extended with the accepted user message. Tool-call
and assistant working context is promoted on successful completion only; cancellation
and failure discard that run's partial model/tool context. Canonical events still
record its observable committed transitions.

## CLI startup and process lifetime

### Normal Agent Server mode (unchanged)

```text
minimal argv → flush line prompt → read input / handle local commands
    → first message → initialize runtime/tracing → connect or spawn Agent Server
    → use existing config, or resolve/apply it in the CLI → session → run
```

V0 is a single-line REPL, not a raw-terminal/full-screen TUI. Help, blank lines,
unknown slash commands, `/new`, and exit before the first message do not create a
runtime, connect, scan a project, or initialize a provider. Messages reuse one
session until `/new`; old sessions remain Agent Server-owned.
Explicit authentication commands also trigger lazy backend initialization, but do
not submit a model message. Browser presentation uses generic reverse RPC; the
Provider never reads the terminal. Input during a run waits until it terminates.
Ctrl-C cancels the active run/auth operation, or exits when idle. During authentication,
EOF or `/quit` cancels and exits; other input is ignored.

A bounded input channel fed by a detached OS thread keeps blocked stdin reads out
of Tokio's shutdown path. The idle loop drains events and notices connection loss
without another keystroke. Configuration and run errors return to the prompt;
connection loss and 10-second Agent Client SDK command deadlines exit without
automatic retry.
Inference has no CLI-wide deadline. There is no line editor, persistent input
history, completion, or streaming rendering. Backend startup waits for a readiness
record, not PID/process scanning or a polling sleep. The Agent Server is explicitly
unmanaged and survives CLI exit. The printed endpoint allows another protocol
Client to connect directly to that Agent Server. V0 has no idle shutdown, session browser,
or daemon discovery. Explicitly stop the process when finished; its printed PID is diagnostic only. The Agent Server does not receive a hidden
shutdown command on disconnect. Spawned Agent Servers use a separate process group
so terminal Ctrl-C reaches the CLI's cancellation handler, not the Agent Server.

### Explicit direct mode

`moly --direct` is opt-in and incompatible with `--connect`; there is no fallback
from Agent Server mode or to another model. The first prompt and local commands remain
lazy. The first authentication/model operation resolves the same Provider profiles,
explicit environment, and nonsecret registration policy, then invokes the MPP SDK
without spawning or contacting an Agent Server.

The CLI owns login/status/logout UI, a memory-only credential slot, and multi-turn
context with opaque metadata. Credentials survive `/new` and fresh Provider children,
but vanish on CLI exit. Only nonsecret host identity/registration persists, with
the same Unix owner-only permissions and conflict protections as normal mode.
Successful turns promote their model context; failed/cancelled turns discard
working context. A fresh local SessionId on `/new` has no Agent Server session authority;
turns use fresh RunId/ModelCallId correlations.

Initial direct mode advertises no tools and rejects requested tools without effects.
It has no agentic loop, Agent Server sessions, event replay, saved-session access,
Session Store connection, or multi-client continuity. Input queued during inference
waits for completion; Ctrl-C drops the active SDK operation and kills its child.
During authentication, EOF or quit cancels/exits and other input is ignored.
When idle, `/new` resets conversation only, not login. Direct mode does not provide
a persistent service after CLI exit.

## Deliberate limits and pressure points

- No Daemon yet: no Agent Server adoption, Daemon supervision/restart, or scheduler.
  Checkpoints, persistence, durable ACKs, and multi-agent orchestration are also absent. Logical identities and
  multiple direct connections to the Agent Server leave room to add it as an ordinary
  Agent Client SDK consumer; direct CLI chat is not a multi-client service.
- No remote access policy, TLS gateway, TCP listener, WebSocket, or Client–Agent Server
  STDIO RPC (Provider STDIO is supported). Local peers are trusted and equally authoritative. Unix endpoints belong in an
  owner-only directory. Windows pipe ACL behavior needs platform/security review
  before use across trust boundaries.
- Agent Server event history is a bounded in-memory suffix, not durability. Disconnection never
  deletes a session or cancels a run. Slow consumers close instead of silently
  dropping events; reconnect explicitly with the last observed sequence.
- Agent Server sessions and conversation context are memory-resident and currently have no
  quota, deletion, compaction, or expiration. This is a local experiment, not a
  hardened multi-tenant service.
- No automatic reconnect, request deduplication, or exactly-once command delivery.
  A lost response leaves command outcome uncertain: inspect/replay, do not blindly
  retry side effects. Request cancellation is not semantic run cancellation.
- Tool registrations are connection-scoped and session-specific. No placement
  policy, tool input-schema validation engine, tool-output streaming, retries,
  process trees, or persistent executors. An executor disappearing during work
  fails the run rather than migrating its side effect. Only `read_file` is bundled;
  there is no directory-listing capability. Tool events do not carry outputs or an
  explicit success/failure flag; known execution failures are carried in model context.
- Config updates have CAS but no `config.changed` subscription yet. Secret updates
  are separate and serialized per reference; a config revision does not freeze secret
  contents. A committed rotation survives cancellation of the surrounding operation.
- Authentication is a same-machine, single-profile pilot. No remote callback routing,
  device-code login, durable credentials, or cross-host refresh coordination. Host
  identity and nonsecret registration metadata are Client-owned persisted config,
  not conversation persistence. Agent Server auth IDs are connection-scoped and bounded;
  direct auth uses local attempt identities and inline callbacks. See
  [routing and limits](authentication.md#routing-cancellation-and-limits).
- MPP v2 (Provider semantics) supports cross-language extensions and scoped reverse
  interaction/credential services but is experimental, not a stable API. No persistent
  Provider workers, dynamic Rust ABI, WASM, cluster, or consensus abstraction.

These are scope limits, not claims that the architecture has already failed in
production. Record measured pressure before adding machinery.

## Lessons from recorded implementation checkpoints

Earlier architectural lessons remain applicable. Current MPP extraction checks
are recorded separately in [verification.md](verification.md).

- The initial five-crate packaging obscured application ownership: the CLI package
  imported Agent Server internals and housed both binaries. Role consolidation made the
  process boundary structural. The subsequently requested Client SDK extracts only
  client-side code; it does not reintroduce a shared runtime. A Cargo-metadata
  regression now guards the role dependency graph, and SDK fixtures spawn the actual
  Agent Server instead of linking it. Provider extraction removes HTTP from Agent Server dependencies.
  Agent Server protocol v2 intentionally changes resolved configuration; envelope v1 and
  the original trace fixture remain preserved. The sixth crate adds an independent
  OAuth/Responses Provider with Provider v2 and Agent Server v3; historical schemas/traces
  stay at the root. The subsequent MPP SDK adds a seventh crate for a real shared
  host boundary, not a shared agent runtime. The dependency regression and independent
  host/direct CLI process tests exercise that extraction. These packaging changes
  do not require a new archive generation.
- Logical lifetime separation is insufficient if the spawned Agent Server inherits the
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
  handlers and any captured Client cycles. The Agent Client SDK closes that
  connection on interrupted registration: it cannot know which registry the Agent Server committed.
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
