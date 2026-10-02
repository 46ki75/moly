# Experimental Model Provider protocol v1

This is an implementation checkpoint for [the requirements](REQUIREMENTS.md), not
full requirements compliance. A Provider can be implemented in another language
without linking Rust, rebuilding the Server, WASM, or a native ABI.

The language-neutral contract is the [JSON Schema](../conformance/schemas/model-provider-v1.json)
plus the semantics below. Rust types live in `moly-protocol::model`. Schema
`$defs` name individual method payloads. Schema validation alone cannot enforce
request correlation, advertised-tool membership, process ownership, or ordering.

## Version and channel

The common envelope remains version **1**. Provider semantics independently use
version **1**; Client–Server semantics now use **2** because resolved configuration
changed. These versions are not interchangeable. Unsupported versions are rejected,
not silently interpreted as the current version. Unknown fields are tolerated;
unknown methods return `unknown_method`. Unknown outcomes fail explicitly.

This implementation uses child stdin/stdout with bounded UTF-8 JSONL, one object
per LF-terminated frame, maximum 1,048,576 bytes before LF. Context and opaque
metadata share this limit; oversized requests fail rather than silently dropping
history or metadata. Partial EOF, malformed JSON, unsupported envelope versions,
and oversized frames fail the connection.
Correlation IDs are positive unsigned 64-bit integers; implementations must preserve
their precision. A response echoes its request ID. Each side has one writer.
There are no Provider events or reverse host-service requests in this version.

The Server sends the initial request:

```json
{"version":1,"type":"request","id":1,"method":"initialize","params":{"protocol_version":1}}
{"version":1,"type":"response","id":1,"result":{"role":"model_provider","protocol_version":1}}
```

Normal operations require initialization. Reinitialization returns
`already_initialized`; pre-handshake operations return `not_initialized`.
The Server validates both role and semantic version. This handshake direction is
an explicit protocol choice, not a consequence of who spawned the process.

| Method | Parameters | Result |
| --- | --- | --- |
| `initialize` | `Initialize` | `Initialized` |
| `provider.validate` | Provider-specific options | `null`, or structured error |
| `provider.step` | `ModelRequest` | `ModelStep`, or structured error |

A structured error uses `type: error`, the request's `id`, and
`error: {code, message}`. Uncorrelated framing failures may omit the ID or use null.
Validation checks options without invoking a model or obtaining credentials.
It is not proof that later authentication or model access will succeed.

## Configuration and process ownership

Client–Server v2 resolved configuration has this shape:

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

The Client resolves the executable path and arguments. The Server does not search
PATH, invoke a shell, expand environment variables, discover configuration, or
fall back to another Provider. Options are opaque to Core; the selected Provider
validates them. `config.apply` validates before committing a CAS revision.
The JSON command can launch an interpreter with a script argument just as it can
launch a native executable. A pre-existing Provider service is not needed or used.

The child receives only its explicitly configured environment, not the Server's
inherited environment. Supply necessary runtime variables explicitly (for example,
`SystemRoot` on Windows). Configuration is visible to attached trusted Clients;
put credentials in `secret.put` and refer to them by `secret_ref`, not in options,
arguments, or environment. Only the selected credential value is sent in the
private `provider.step` payload. It is neither part of model context nor saved
history. This memory-only credential mechanism is not a zeroizing vault.

**Current hosting policy:** one fresh child per validation or model step, initialized
lazily and never reused. It handles at most one outstanding request at a time.
This is a small implementation choice, not a required process cardinality.
The Server kills the child after the operation and allows one second for reaping.
Dropping an in-flight Server operation kills its child; Tokio performs best-effort
reaping on cancellation.
Run cancellation fences late results independently of physical termination.
Client disconnection does not cancel a Server-owned model call.
The bundled Provider also watches stdin EOF during HTTP work, so Server death
aborts that work rather than waiting for its HTTP timeout.

Handshake and validation each have a three-second deadline. A model step has a
65-second protocol deadline; the bundled HTTP request/body deadline is 60 seconds.
A failed or interrupted operation is never automatically retried. Process loss is
not proof that the upstream request had no effect. Process-tree sandboxing and
termination of arbitrary grandchildren are not provided.

## Model semantics

`ModelRequest` contains opaque `options`, nullable `credential`, logical `context`
(`session_id`, `run_id`, `model_call_id`, `call_kind: primary`), normalized `messages`,
and advertised `tools`. The Server owns those identities, step ordering, and tool
execution authority.

Supported context messages:

- `user`: `text`.
- `assistant`: nullable `text`, `tool_calls`, nullable `metadata`.
- `tool_result`: `call_id` and structured JSON `output`.

A tool call is `{id, name, arguments}` with object-valued arguments. A tool result
is **not** an OpenAI stringified tool message; each Provider performs its own
upstream encoding. Metadata is `{format, value}` and remains opaque to Core.
It is replay data, not an instruction for Core to execute anything.

Outcomes:

- `completed`: `text` and optional/nullable `metadata`.
- `await_host_tools`: optional/nullable `text`, `calls`, optional/nullable `metadata`.

The Server validates all requested tools before dispatching any: 1–32 calls,
nonempty unique call IDs, advertised names, and JSON object arguments. It assigns
independent ToolRun identities and execution leases. Provider call IDs alone give
no side-effect authority. A completed response may contain an explicitly empty
text string; missing final text is invalid.

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

The Client supplies the endpoint/model and secret reference; Provider validation
still needs no credentials or model access. A Go model step requires a bearer
credential. The Provider derives `x-opencode-session` from `context.session_id` and
sends `User-Agent: moly/<version>`, as required by the
[official Go guide](https://opencode.ai/docs/go#where-can-i-use-it). Run IDs, model
call IDs, and child process lifetimes never replace conversation identity. No
Server Core or SDK-specific Go behavior is involved.

Selection is explicit, not inferred from URL substrings. A configured proxy or
mock endpoint receives the same Go headers; ordinary `openai` requests do not get
the session header. Redirects remain disabled to avoid forwarding identifiers,
credentials, or prompts elsewhere. Go's Responses and Anthropic Messages endpoints
are not supported by this Chat Completions adapter. No protocol version or process
packaging change is required for this implementation-specific profile.

Streaming, usage accounting, native features, generic continuation outcomes,
interactive authentication, and reverse host services are not implemented here.
Session Store protocols, tree persistence, and restoration remain a separate
implementation checkpoint. Normalized model context is not a substitute for the
required append-only history format.

## Errors and diagnostics

Server-side launch/transport categories include `invalid_config`,
`provider_unavailable`, `provider_timeout`, `provider_protocol`, and
`provider_request_too_large`. The Server recognizes a bounded set of semantic
Provider error codes and replaces their messages with static descriptions. Unknown
codes become `provider_error`. It never forwards raw Provider codes, messages,
HTTP bodies, or stderr: even a correctly framed error may contain credentials.
Provider subprocess stderr is discarded in this initial supervisor policy.

Rust executable roots install stderr tracing subscribers. Core/transport tracing
contains logical IDs and lifecycle transitions, not prompts, options, arguments,
results, or credentials. Third-party processes are trusted executable code, not
sandboxed plugins; their internal behavior cannot be made safe by JSON framing.

## Verification

The existing Client/Server framing, duplex, event-ordering, cancellation, and REPL
suites remain applicable. The original v1 trace is retained with its historical
configuration schema; it is not a claim that live Servers still accept v1 clients.
Cross-language tests run a Python standard-library Provider through the actual
Server and public SDK, using synthetic data and fake credentials without external
model access. They require Python 3 and fail actionably rather than skipping if
it is missing.
Bundled-provider HTTP tests use local mock servers, not live services.
