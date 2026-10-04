# Experimental Moly Provider Protocol (MPP) v2

MPP is the host–Model Provider contract for model steps and authentication, not an
agentic loop. A host may be the Agent Server or the explicit `moly --direct` CLI;
hosting does not create a new Component role or transfer Agent Server-run authority
to a Client. The Agent Server owns the agentic loop; direct CLI chat is model-only. A Provider can be implemented in another language
without linking Rust, rebuilding either host, WASM, or a native ABI.
This is an implementation checkpoint for [the requirements](REQUIREMENTS.md), not
full requirements compliance. See [verification results and limits](verification.md).

The language-neutral contract is the [JSON Schema](../conformance/schemas/model-provider-v2.json)
plus the semantics below. Rust types live in `moly-protocol::model` and `::auth`. Schema
`$defs` name individual method payloads. Schema validation alone cannot enforce
request correlation, advertised-tool membership, process ownership, or ordering.
The documentation URL `docs/model-provider-protocol.md`, schema filenames
`model-provider-v1.json` / `model-provider-v2.json`, and historical assets are retained;
the MPP name is not a wire or history migration.

## Version and channel

The common envelope remains version **1**. MPP (Provider) semantics independently
use version **2**; Client–Agent Server semantics use **3**. The handshake role remains
`model_provider`. Provider v2 adds explicit authentication, reverse interactions,
and scoped credential replacement; naming MPP and sharing a host SDK change none
of these wire contracts. V1 Provider and v1/v2 Agent Server semantics are rejected, not
silently upgraded. Envelope, Provider, and Agent Server versions are not interchangeable. Unknown protocol object fields are
tolerated; Provider-specific options may be strict. Unknown methods return
`unknown_method`. Unknown outcomes fail explicitly.

This implementation uses child stdin/stdout with bounded UTF-8 JSONL, one object
per LF-terminated frame, maximum 1,048,576 bytes before LF. Context and opaque
metadata share this limit; oversized requests fail rather than silently dropping
history or metadata. Partial EOF, malformed JSON, unsupported envelope versions,
and oversized frames fail the connection.
Correlation IDs are positive unsigned 64-bit integers; implementations must preserve
their precision. A response echoes its request ID. Each side has one writer.
There are no Provider events. Reverse host-service requests are permitted during
an initialized operation, not during handshake. Each direction has independent IDs;
the current host permits at most 64 strictly increasing reverse IDs per operation.

The host sends the initial request:

```json
{"version":1,"type":"request","id":1,"method":"initialize","params":{"protocol_version":2}}
{"version":1,"type":"response","id":1,"result":{"role":"model_provider","protocol_version":2}}
```

Normal operations require initialization. Reinitialization returns
`already_initialized`; pre-handshake operations return `not_initialized`.
The host validates both role and semantic version. This handshake direction is
an explicit protocol choice, not a consequence of who spawned the process.

| Method | Parameters | Result |
| --- | --- | --- |
| `initialize` | `Initialize` | `Initialized` |
| `provider.validate` | Provider-specific options | `null`, or structured error |
| `provider.step` | `ModelRequest` | `ModelStep`, or structured error |
| `provider.auth` | `ProviderAuthRequest` | `AuthStatus`, or structured error |
| `host.interact` (reverse) | `InteractionRequest` | `InteractionResponse` |
| `host.credential.replace` (reverse) | `CredentialReplace` | null |

Authentication is optional for an implementation: the API-key Provider returns
`auth_unsupported`. [Authentication semantics](authentication.md) specify method
payloads, Agent Server-path Client routing or direct host interaction, scoped credential
authority, cancellation, and deadlines.
These are generic host services, never model-requested tools.

A structured error uses `type: error`, the request's `id`, and
`error: {code, message}`. Uncorrelated framing failures may omit the ID or use null.
Validation checks options without invoking a model or obtaining credentials.
It is not proof that later authentication or model access will succeed.

## Configuration and process ownership

Client–Agent Server v3 resolved configuration retains the v2 shape below. In direct mode,
the CLI resolves the same Provider command/options itself; `workspace` and
`secret_ref` do not create an Agent Server or Session Store relationship:

```json
{
  "provider": {
    "command": {
      "executable": "/absolute/path/to/moly-provider-openai",
      "args": [],
      "env": {}
    },
    "options": {
      "model_endpoint": "https://api.openai.com/v1/chat/completions",
      "model": "gpt-4.1-mini"
    }
  },
  "workspace": "/absolute/workspace",
  "secret_ref": "my-model-key"
}
```

The Client resolves the executable path and arguments. The common MPP SDK does
not search PATH, invoke a shell, expand environment variables, discover configuration,
or fall back to another Provider. Options are opaque to both hosts; the selected
Provider validates them. In Agent Server mode, `config.apply` validates before committing
a CAS revision. Direct mode has no Agent Server config revision/CAS authority.
The JSON command can launch an interpreter with a script argument just as it can
launch a native executable. A pre-existing Provider service is not needed or used.

The child receives only its explicitly configured environment, not either host's
inherited environment. Supply necessary runtime variables explicitly (for example,
`SystemRoot` on Windows). Agent Server configuration is visible to attached trusted Clients;
in Agent Server mode put credentials in `secret.put` and refer to them by `secret_ref`,
not in options, arguments, or environment. The direct CLI instead owns its selected
memory-only credential slot; it makes no `secret.put` call. Only the selected value
is sent in private `provider.step` or `provider.auth` payloads. It is neither part
of model context nor saved history.
The host can accept scoped credential replacement during login/logout or model
refresh, but never validation/status. Hosts must serialize operations sharing a
renewable credential for their full lifetime. Credentials disappear on Agent Server
restart in normal mode or CLI exit in direct mode. Neither store is a zeroizing vault.

**Current hosting policy:** one fresh child per validation, authentication operation,
or model step, initialized lazily and never reused. It handles one outstanding
host operation at a time; that operation may make sequential reverse host calls.
This is a small implementation choice, not a required process cardinality.
The MPP SDK supervises the child on behalf of its caller, kills it after the operation,
and allows one second for reaping. Dropping an in-flight operation kills that child;
Tokio performs best-effort reaping on cancellation. No detached worker or process
reuse is provided. Agent Server-routed children remain Agent Server-owned; direct CLI children
are independent and CLI-owned.
Agent Server run cancellation fences late results independently of physical termination.
Client disconnection does not cancel an Agent Server-owned model call, but does cancel
that connection's explicit authentication operations. In direct mode cancellation
or CLI exit drops its own operation instead; there is no surviving Agent Server run.
Both bundled Providers watch stdin EOF during HTTP work, so host death aborts
that work rather than waiting for its HTTP timeout.

Handshake and validation each have a three-second deadline. A model step has a
65-second total deadline; the Agent Server includes credential-lease contention in that
budget. The Chat Completions HTTP request/body deadline is 60 seconds. Login has a
300-second total deadline; status/logout have 30 seconds, independent of model
timeouts. SDK deadlines include startup/callbacks, not the caller's wait to acquire
a credential lease; the Agent Server additionally bounds that wait.
A failed or interrupted operation is never automatically retried. Process loss is
not proof that the upstream request had no effect. Process-tree sandboxing and
termination of arbitrary grandchildren are not provided.

## Common Rust host SDK

[`moly-provider-client`](../crates/moly-provider-client/README.md) is an experimental
convenience API over MPP, not the MPP contract or an Agent Client SDK. It depends only on `moly-protocol` among project
crates and re-exports it as `protocol`. The Agent Server and direct CLI share this host
implementation; independent Providers still need no Rust linkage.

Stateless `ProviderClient` provides `validate`, `authenticate`, and `step`. Each
method takes a resolved `ProviderConfig` and launches an initialized child. The
SDK derives request options from config and credentials from the borrowed slot;
a `step` without a credential scope sends `credential: null`. Status can read but
not replace; login/logout/step can commit scoped replacement immediately. Later
failure or cancellation does not roll back that replacement. The SDK has no secret
store: hosts must hold exclusive use of a renewable credential for the whole call.

`Interaction::new(callback)` supplies user-facing presentation for authentication.
The callback is permitted only for login, runs inline, returns an
`InteractionOutcome`, and is dropped with cancellation; the SDK supplies the matching
attempt ID. Model steps cannot invoke interaction. Framing, supervision, limits, and sanitized `ProtocolError` failures
remain private machinery. Configuration discovery, credential persistence, UI,
conversation history, tools, and agent/run policy stay outside this SDK; it has
no agentic loop or Session Store access.
The Agent Client SDK, `moly-client`, instead connects to Agent Server sessions/runs and
never spawns processes.

## Model semantics

[Conversation history v1](conversation-history.md) is a separate storage data
contract. On restoration, its stable session ID supplies `context.session_id`;
selected history must be explicitly projected to model context. MPP requests are
not history files. The history codec does not implement activation or restoration,
and stored Provider continuation state adds no MPP methods or automatic reuse.

`ModelRequest` contains opaque `options`, nullable `credential`, logical `context`
(`session_id`, `run_id`, `model_call_id`, `call_kind: primary`), normalized `messages`,
and advertised `tools`. For Agent Server runs, the Agent Server owns those identities,
step ordering, and tool execution authority. A direct CLI supplies local conversation
and fresh turn/model correlations, not Agent Server session/run authority.

Supported context messages:

- `user`: `text`.
- `assistant`: nullable `text`, `tool_calls`, nullable `metadata`.
- `tool_result`: `call_id` and structured JSON `output`.

A tool call is `{id, name, arguments}` with object-valued arguments. A tool result
is **not** an OpenAI stringified tool message; each Provider performs its own
upstream encoding. Metadata is `{format, value}` and remains opaque to either host.
It is replay data, not an instruction for the host to execute anything.

Tool `output` may contain a known execution failure such as
`{"error":{"code":"tool_file_not_found","message":"Requested file was not found"}}`.
This is a correlated `tool_result`, not a failed `provider.step`. Providers encode
it just like any other tool output; Core neither parses it nor automatically retries
the tool. Every call in an accepted batch receives its result before inference
continues, unless a runtime failure or cancellation terminates the run. See
[tool execution outcomes](protocol.md#tool-execution-outcomes).

Outcomes:

- `completed`: `text` and optional/nullable `metadata`.
- `await_host_tools`: optional/nullable `text`, `calls`, optional/nullable `metadata`.

The host must validate all requested tools before dispatching any: 1–32 calls,
nonempty unique call IDs, advertised names, and JSON object arguments. The MPP SDK
enforces this for returned batches but never executes tools. The Agent Server assigns
independent ToolRun identities and execution leases. Initial direct CLI
mode advertises `tools: []` and rejects `await_host_tools` without performing any
effect; it does not gain an agent/tool loop from the SDK. Provider call IDs alone
give no side-effect authority. A completed response may contain an explicitly empty
text string; missing final text is invalid.

Direct chat preserves accepted assistant metadata and normalized context across
successful turns, discarding failed/cancelled turn working context. `/new` resets
the local conversation/SessionId without clearing authentication. There are no
Agent Server sessions, canonical events/replay, saved-session access, or multi-client
continuity. `--direct` and `--connect` are incompatible; errors never trigger a
fallback to another host or model.

The bundled implementation supports nonstreaming OpenAI-compatible Chat Completions.
Its options require an HTTP(S) endpoint without embedded credentials/fragments and
a nonblank model name of at most 256 bytes. Endpoint paths are used exactly as
supplied. It disables redirects and ambient proxies, caps responses at 512 KiB,
and rejects truncation, unsupported native features, and malformed tool calls.
It preserves accepted assistant messages under metadata format
`openai.chat_completion.message.v1`; unsupported or inconsistent replay metadata
fails rather than being silently discarded.

The compatible adapter also preserves string/null `reasoning_content` in opaque
replay metadata without displaying it as assistant text. This is needed for
[thinking-model tool continuation](https://api-docs.deepseek.com/guides/thinking_mode/#tool-calls).
Other unsupported fields and malformed reasoning values still fail explicitly.

### OpenCode Go profile

The same executable accepts optional `options.profile`, a JSON string: `openai`
(default) or `opencode-go`. Unknown profiles and non-string values are
`invalid_config`, not fallback. For Go, supply:

```json
{
  "profile": "opencode-go",
  "model_endpoint": "https://opencode.ai/zen/go/v1/chat/completions",
  "model": "kimi-k2.6"
}
```

The Client supplies the endpoint/model and selects a credential (an Agent Server secret
reference or direct local slot); Provider validation still needs no credentials or
model access. A Go model step requires a bearer credential. The Provider derives
`x-opencode-session` from `context.session_id` and sends `User-Agent: moly/<version>`,
as required by the
[official Go guide](https://opencode.ai/docs/go#where-can-i-use-it). Run IDs, model
call IDs, and child process lifetimes never replace conversation identity. No
Agent Server Core or SDK-specific Go behavior is involved.

Selection is explicit, not inferred from URL substrings. A configured proxy or
mock endpoint receives the same Go headers; ordinary `openai` requests do not get
the session header. Redirects remain disabled to avoid forwarding identifiers,
credentials, or prompts elsewhere. Go's Responses and Anthropic Messages endpoints
are not supported by this Chat Completions adapter. No protocol version or process
packaging change is required for this implementation-specific profile.

### ChatGPT OAuth / Responses Provider

The separate `moly-provider-openai-codex` implements the documented ChatGPT OAuth
and public Responses route; see [interactive authentication](authentication.md).
Its upstream SSE is buffered into these same model outcomes, not forwarded as
Client-visible streaming. It does not embed the Codex agent or execute tools itself.
Options are `{model, host_id, registration?}`: a nonblank model of at most 256 bytes,
a stable canonical UUIDv4 `urn:uuid:` host ID, and optional nonsecret registration
from a previous successful auth result. Model/issuer endpoints are not configurable
in production. The CLI requires explicit `MOLY_MODEL` and `MOLY_AUTH_STATE_FILE`.

Responses HTTP has a 60-second deadline, a 2-MiB stream limit, a 512-KiB per-event
limit, and a 768-KiB normalized result limit, in addition to the shared 1-MiB STDIO
frame limit. Partial UTF-8/SSE frames are buffered; missing terminal completion is
failure. Only text, hosted function calls, and supported opaque reasoning replay
are accepted. Unsupported native features and replay formats fail explicitly.

Client-visible streaming, usage accounting, native features, and generic continuation
outcomes remain unimplemented.
Session Store protocols, tree persistence, restoration, and CLI save/resume remain
unimplemented. Session Store access remains Agent Server-only; direct CLI and the
MPP SDK have none. Normalized model context is not a substitute for the
required append-only history format.

## Errors and diagnostics

Host-side launch/transport categories include `invalid_config`,
`provider_unavailable`, `provider_timeout`, `provider_protocol`, and
`provider_request_too_large`. The MPP SDK recognizes a bounded set of semantic
Provider error codes and replaces their messages with static descriptions. Unknown
codes become `provider_error`. Neither host forwards raw Provider codes, messages,
HTTP bodies, or stderr: even a correctly framed error may contain credentials.
Provider subprocess stderr is discarded in this initial supervisor policy.

Rust executable roots install stderr tracing subscribers. Core/transport tracing
contains logical IDs and lifecycle transitions, not prompts, options, arguments,
results, or credentials. Third-party processes are trusted executable code, not
sandboxed plugins; their internal behavior cannot be made safe by JSON framing.

## Verification

The Client/Agent Server framing, duplex, event-ordering, cancellation, and REPL suites
remain applicable. Independent MPP SDK and direct CLI process tests also pass;
see [verification.md](verification.md) for exact scope and platform limits.
The original v1 trace is retained with its historical configuration schema; it is
not a claim that live Agent Servers still accept v1 clients. Cross-language tests run a
Python standard-library Provider through the actual Agent Server and public Agent Client
SDK, using synthetic data and fake credentials without external model access. They
require Python 3 and fail actionably rather than skipping if it is missing.
Bundled-provider HTTP tests use local mock servers, not live services. Authentication
fixtures use local OAuth/token/JWKS endpoints and synthetic credentials. These checks
do not establish live OpenAI OAuth admission, model entitlement, or service compatibility.
