# Experimental Agent Server protocol v3

Common envelope version: **1**. Client–Agent Server semantic version: **3**.
V1/v2 Clients are rejected at initialization: v3 adds explicit Provider authentication
and connection-scoped interactions. Resolved configuration, event, lease, framing,
and duplex semantics are unchanged from v2.
See the independently versioned [Moly Provider Protocol (MPP) v2](model-provider-protocol.md)
and [authentication semantics](authentication.md). MPP naming and host SDK extraction
change neither envelope v1, Provider v2 (role `model_provider`), nor Agent Server v3.

Agent Server is the component name and owns the agentic loop. Older **Server**
terminology aliases Agent Server. The executable/crate `moly-server`, Rust symbols
(including `ServerId` and `SERVER_VERSION`), wire role `server`, method strings,
schema IDs/filenames, and protocol versions remain unchanged for compatibility.

This document governs the normal Agent Server path, which remains unchanged. Opt-in
`moly --direct` uses MPP without an Agent Server; it provides model-only chat/authentication,
not an agentic loop, and does not implement this protocol or gain Agent Server
sessions, replay, tool execution, or multi-client authority. It is incompatible with
`--connect`, with no automatic fallback. Session Store access and agentic-loop
ownership remain exclusively with the Agent Server. Session Store integration,
conversation persistence, and CLI save/resume are not implemented.

Canonical Rust schemas: `crates/moly-protocol/src/lib.rs`, `model.rs`, and `auth.rs`. Rust source compatibility,
wire compatibility, and future checkpoint compatibility are separate promises.
There is no checkpoint format in the current implementation. JSONL is the initial codec, not the semantic
boundary. A later codec must preserve operations, identities, errors, and ordering.

## Framing and connection policy

- UTF-8 JSON object followed by LF; at most 1,048,576 payload bytes before LF.
- Reads/writes may split or combine arbitrary frames. Embedded newlines are escaped
  within JSON strings. Trailing JSON whitespace (including CR before LF) is accepted.
- Clean EOF between frames closes the connection. Partial EOF, invalid UTF-8/JSON,
  invalid envelope, oversized payload, or unsupported envelope version triggers a
  best-effort uncorrelated `invalid_frame` error and closure. Flushing that error
  has a one-second deadline; an unwritable peer may only observe closure.
- Unknown object fields are tolerated. Unknown methods receive correlated
  `unknown_method` errors. Unknown envelope types are invalid. Clients ignore
  unknown event stream names, but fail closed on an invalid known event payload.
- Exactly one bounded writer queue writes a connection's frames. RPC is duplex:
  outstanding requests, reverse requests, responses, and events can overlap.
- Each direction independently allocates non-reused positive request IDs during a
  connection (the current peers use u64). Requesters must not reuse an outstanding ID.
  Replies to cancelled/no-longer-pending requests are ignored. Peer request deduplication
  is not provided. Browser adapters must preserve integer precision.
- Wire request and application queues have finite bounds. Excess inbound traffic
  or a stalled event consumer closes the connection; this never implies run
  cancellation. No transparent automatic retry occurs.

Envelopes:

```json
{"version":1,"type":"request","id":1,"method":"initialize","params":{"protocol_version":3}}
{"version":1,"type":"response","id":1,"result":{"server_id":"00000000-0000-4000-8000-000000000001","role":"server","protocol_version":3}}
{"version":1,"type":"error","id":2,"error":{"code":"unknown_method","message":"unknown method"}}
{"version":1,"type":"error","id":null,"error":{"code":"invalid_frame","message":"malformed, oversized, or incomplete frame"}}
```

Initialize is required before ordinary commands. Incompatible handshake versions
are rejected; repeated initialization returns `already_initialized`. The Client
verifies role and version, not just the ability to connect. All current local peers
have equivalent authority. There is no daemon-specific code path.

## Operations

| Method | Parameters | Result |
| --- | --- | --- |
| `initialize` | `protocol_version` | `server_id`, `role`, `protocol_version` |
| `config.get` | null | `revision`, optional `config` |
| `config.validate` | resolved config object | null |
| `config.apply` | `base_revision`, `config` | new config snapshot |
| `secret.put` | `key`, `value` (at most 64 KiB) | null; memory-only, trusted local peers |
| `provider.auth` | `attempt_id`, `operation`, `config_revision` | nonsecret `AuthStatus` |
| `auth.cancel` | `attempt_id` | null; initiating connection only |
| `interaction.request` (reverse) | `attempt_id`, HTTPS `url` | matching `attempt_id`, presentation `outcome` |
| `session.create` | null | `session_id` |
| `subscribe` | `session_id`, `after_seq` | `live_head` at attachment |
| `unsubscribe` | `session_id` | null |
| `run.start` | `session_id`, `message` | `run_id` |
| `run.cancel` | `session_id`, `run_id` | null |
| `tools.register` | `session_id`, `tools` | `executor_id` |
| `tool.execute` (reverse) | `session_id`, `lease`, `name`, `arguments` | matching `lease`, `output` |

Resolved config contains `provider: {command: {executable, args, env}, options}`,
absolute `workspace`, and optional `secret_ref`. The executable path is absolute,
arguments are literal, and the environment is explicitly supplied rather than
inherited. Provider options are opaque to Core. The Chat Completions Provider accepts
`options: {model_endpoint, model, profile?}`, where `profile` is `openai` (default)
or `opencode-go`; see the [configuration example](model-provider-protocol.md#configuration-and-process-ownership)
and [Go profile](model-provider-protocol.md#opencode-go-profile).
Validation invokes the selected implementation's `provider.validate`, without a
model call or credentials. It is not configuration discovery or model connectivity
validation. The first apply uses revision zero. A conflict fails without mutating
the active snapshot. Runs retain their accepted config revision/snapshot.
Authentication separately pins that snapshot and requires a selected credential reference; see
[authentication routing, cancellation, and limits](authentication.md). It is not a
hosted model tool and creates no canonical conversation events.

`tools.register` replaces that connection's registrations for one session. Each
tool has `name`, `description`, `input_schema`. `read_file` is reserved only when
the local executor is composed into the Agent Server. Registration during an active run
returns `run_busy`. A tool callback may issue ordinary commands using the same
Client; it does not block reverse-response correlation. Leases bind
ToolRunId, ExecutorId, and generation; the outstanding Agent Server assignment also binds
arguments/name. Responses with mismatched authority cannot commit tool completion.

The bundled Providers support hosted function calls and final text. The separate
Codex/ChatGPT Provider consumes upstream Responses SSE internally; this does not
add Client-visible streaming. Native provider features are a different execution
class and currently unsupported. A run may make up to 16 model steps; a provider
step may request up to 32 sequential hosted tools.

### Tool execution outcomes

A known execution failure is an ordinary `tool.execute` response with the original
lease and structured `output`, not a protocol error. The recommended failure output is:

```json
{"error":{"code":"tool_file_not_found","message":"Requested file was not found"}}
```

Core keeps `output` opaque. The executor classifies failures and supplies sanitized
messages without credentials, host paths, or native diagnostics. Each result retains
the originating Provider call ID in model context. Unless a runtime fault or
cancellation terminates the run, all calls in an accepted batch return results
before the next model step; the model may correct a request or ask for help.
The Agent Server does not automatically retry tools.

The bundled `read_file` returns `{"content": string}` on success and the failure
shape above for invalid arguments, path refusals, or read errors:

| Failure | Output error code |
| --- | --- |
| Invalid arguments | `invalid_params` |
| Path outside workspace | `tool_path_outside_workspace` |
| Nonregular file | `tool_invalid_path` |
| Missing file | `tool_file_not_found` |
| Permission denied | `tool_permission_denied` |
| Other file I/O error | `tool_error` |
| Invalid UTF-8 | `tool_invalid_utf8` |
| File larger than 64 KiB | `tool_result_too_large` |

Invalid workspace configuration, stale/mismatched execution authority, executor
loss/timeouts, malformed protocol/results, and reverse RPC errors still fail the
run. Cancellation remains terminal. These faults are not converted into model-facing
results. Malformed Provider batches, including non-object arguments or
unadvertised tools, remain rejected before any tool effects. Client-hosted tools
opt into recovery by returning failure data inside `output`. An Agent Client SDK
tool callback `Err(ProtocolError)` remains fatal, even with a code such as `tool_error` (the current
Agent Server adapter redacts reverse RPC failures to `executor_lost`).

This intentionally changes bundled `read_file` failure behavior without changing
wire shapes, successful outputs, or protocol versions.

## Canonical events and ordering

Events use `type: event`, `event: session.event`, and a typed `params` object with
`session_id`, `seq`, and `kind` plus kind-specific fields. The session actor assigns
contiguous sequence numbers at commit, starting with `session_created` at 1.
Network arrival across connections is not a semantic ordering guarantee.

A successful hosted-tool run follows:

```text
session_created
message_accepted
run_started(config_revision)
model_call_started(ModelCallId)
tool_started(ToolRunId, ExecutorId, generation)
tool_completed(matching lease)
model_call_started(new ModelCallId)
assistant_message
run_completed
```

`tool_completed` means a matching result was committed, including a known execution
failure; it does not assert that the requested operation succeeded. The event does
not carry the result or a success flag. Known failures are conveyed in model context.

`run_failed` and `run_cancelled` are alternative terminal transitions. Outstanding
ToolRuns become unsuccessful when their owning run terminates; the current implementation does not emit a
separate terminal event for each abandoned tool. Late completions are fenced by
both RunId and lease. Cancel does not undo external effects.

Subscribe replay and live attachment are a single mailbox operation: events after
`after_seq` are replayed before subsequent live events, without a gap. Events may
arrive before the subscribe or run-start response. Re-subscribing can intentionally
redeliver events; consumers track per-session sequence. The Agent Server retains the most recent
1,024 events. Too-old cursors return `replay_unavailable`; cursors ahead of the live
head return `invalid_seq`. There is no durable head, persistence, or retention
promise across Agent Server restart.

Common error codes: `not_initialized`, `incompatible_version`, `unknown_method`,
`invalid_params`, `invalid_config`, `revision_conflict`, `not_found`, `run_busy`,
`not_active`, `not_configured`, `invalid_seq`, `replay_unavailable`, `invalid_tool`,
`unknown_tool`, `executor_lost`, `stale_tool_result`, `tool_timeout`, `step_limit`,
`slow_consumer`, and provider/executor-specific failures. Consumers must tolerate
new codes and must not parse human-readable messages.

## Compatibility evidence

Root `conformance/` traces use fixed logical IDs and explicit sequence numbers.
Tests exercise fixtures independently of Agent Server process IDs and endpoint locations.
The same framing and duplex suites exercise the Agent Client SDK's and Agent Server's
private local-IPC transports. Agent Client SDK end-to-end tests spawn the actual
`moly-server` executable and exercise its listener, provider HTTP boundary, state
machine, and executors with deterministic
local fixtures. Neither SDK nor CLI links Agent Server implementation code. The common
MPP host SDK, `moly-provider-client`, has a separate private Provider STDIO transport;
the Agent Server uses it without importing the Agent Client SDK. Separate independent-host
and direct CLI process checks are recorded in [verification.md](verification.md).
The preserved v1 trace uses its historical configuration DTO, not the live v3
configuration schema. The v1 Provider JSON schema is also retained as a historical
contract; live Providers use v2. Cross-language Provider tests use a Python implementation
spawned by the Agent Server and exercised through the public Agent Client SDK. MPP has a
language-neutral JSON Schema; there is no whole-system schema generator or
separately packaged conformance runner yet.
