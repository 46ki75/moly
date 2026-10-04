# Conversation history v1

Status: experimental **data contract**, independently versioned from MPP and the
Agent Server protocol. This does not implement a Session Store, persistence,
CLI save/resume, context reconstruction, or runtime recovery. Read
[CONTRIBUTING.md](../CONTRIBUTING.md) before changing it.

The [JSON Schema](../conformance/schemas/conversation-history-v1.json) describes
individual records. Rust types and pure validation live in
`moly_protocol::history`. Neither SDK owns history storage. In Agent Server mode,
the Agent Server owns activation, branch selection, and the agentic loop; only it
may use a Session Store. Direct CLI mode remains ephemeral, with no Session Store
access. Handling exported data is not permission to mutate a live session.

## Three separate contracts

| Contract | Responsibility |
| --- | --- |
| Conversation history | Durable conversation identity, typed entries, branches, selected head, replay data |
| MPP | Inference input/results, host session identity, Provider-specific encoding |
| Session Store protocol (not implemented) | Discovery, saving/loading, ownership, revisions, durability |

MPP remains envelope v1 / Provider v2; the Agent Server protocol remains v3.
History v1 is not a capture of those envelopes or an alias for `ModelRequest`.
Its message types are independently defined, even where their current shapes are
similar to MPP. Shared `SessionId` values do not couple format versions.

## Portable JSONL snapshot

A snapshot consists of one `session` record, zero or more `entry` records, then
exactly one `selection` record. Every record is a UTF-8 JSON object terminated by
LF; CRLF is accepted and output uses LF. Blank lines, partial final records,
trailing records, duplicate object fields, unknown fields outside opaque data,
and unsupported record/data kinds fail rather than being silently discarded.

- `session`: `format_version: 1`, canonical non-nil UUID `session_id`, integer
  `created_at_ms` (Unix milliseconds), `scope` (`full_tree` or `selected_branch`),
  optional nullable `name`, `workspace`, and `forked_from`.
- `entry`: canonical non-nil UUID `id`, required nullable `parent_id`, positive
  integer `sequence`, integer `timestamp_ms`, and typed `data`.
- `selection`: required nullable `head` and integer `revision`.

The Rust `ConversationHistory` groups these three parts in memory. Its strict
`from_jsonl`, `validate`, `selected_path`, and `to_jsonl` operations are pure:
no file I/O, credential loading, Provider launch, model invocation, or tool effects.
Serde decoding individual records alone is not semantic validation.

`null` parent/head means the virtual root represented by the session header.
Multiple root children are valid branches of that root. IDs are stable logical
identities, not paths, connections, process IDs, or upstream thread handles.
Entry sequences are unique Agent Server-assigned commit positions; a parent must
have a lower sequence than its child. Gaps are allowed, including in branch-only
exports. Timestamps are informational and need not be monotonic.

Record position never determines ancestry or the selected head. Input entry
records may be physically reordered; canonical output sorts them by sequence.
Every parent and selected head must resolve within the snapshot. The selection
revision must be at least every entry sequence. It may advance without adding an
entry, for example after a head change. The revision is a logical snapshot
boundary, not proof of a durable write or a live ownership lease.

A `selected_branch` snapshot contains exactly the selected head's ancestry, with
all parent references preserved. A `full_tree` snapshot can contain abandoned
branches, including when the selected head is the virtual root. The scope is a
producer declaration: a loader cannot prove that an export omitted no remote data.

This is a **consistent export snapshot**, not an append journal. Its final
selection record is not a commit protocol, checksum, or crash-recovery mechanism.
Store implementations may use other layouts. Atomic publication, ownership,
concurrency, incremental saving, and recovery require the separate Session Store
contract; do not append to these snapshots as if that supplied durability.

## Typed entries

The initial supported kinds are:

- `user`: `text`.
- `assistant`: nullable `text`, `tool_calls`, optional nullable `metadata` and
  `attribution`. Calls contain `id`, `name`, and object-valued `arguments`.
  Attribution contains nonsecret `provider`/`binding` identifiers and a `model`.
  Replay metadata is `{format, value}`, opaque to the Agent Server/Store.
- `tool_result`: `assistant_id`, `call_id`, structured JSON `output`. The referenced
  assistant must be an ancestor and advertise that call. Incomplete call batches
  are legal history; a missing result never authorizes repeating an effect.
- `system_prompt`: `text`.
- `tools`: definitions with `name`, `description`, and object-valued `input_schema`.
- `provider_state`: nonsecret `scope: {provider, binding}` and nullable `state`
  (`{format, value}`). Null explicitly clears earlier state in that scope.
- `session_metadata`: nullable `name`. This changes display metadata, not model input.

User/assistant/tool text may be empty. An assistant must have text (possibly
empty) or at least one tool call. Call IDs must be nonempty and unique within an
assistant; a branch cannot contain two results for the same assistant/call pair.
Unknown semantic records cannot be hidden as ignorable extensions. Rich content,
attachments, compaction, context edits, usage/termination records, and complete
runtime checkpoints are not represented in this first slice. Such histories must
fail as unsupported rather than be converted lossily to text. This is not full
implementation of the [persistence requirements](REQUIREMENTS.md#19-persistence).

## Restoration and Provider sessions

Restoring the same conversation preserves `session_id`; subsequent MPP requests
use it as `context.session_id`. New inference uses new run/model-call identities.
A new conversation or explicit fork uses a new session ID. `forked_from`, when
present, records the source `session_id` and required nullable `entry_id`; this
external provenance reference need not resolve in the exported snapshot.

The selected path, not all stored entries, is the basis for model context.
Restoration must not automatically invoke a model, replay a tool, or resurrect
connections, leases, processes, or interrupted runs. History remains readable
without working model credentials. Activating it requires Agent Server ownership
and additional capability/context validation beyond the pure format validator.

Moly's session ID remains distinct from upstream-issued thread/conversation IDs,
affinity keys, and cache keys. `provider_state` can preserve **noncredential** opaque
continuation information at an explicit history position; it does not promise that
the upstream session still exists. Its `provider` identifies the adapter/state
namespace; `binding` is a stable nonsecret identifier for the relevant account,
endpoint, and configuration scope, not a token or executable path.

Select Provider state only along the chosen ancestry, never by taking the last
physical record. Do not automatically reuse it after a fork, account/configuration
change, rewind, or context edit: remote mutable state can include abandoned turns.
Provider-specific validation/rebinding or an explicit new upstream session is
required. Expiry or unavailable continuation must be reported explicitly, without
silently switching accounts or dropping required replay data.

MPP already carries host session IDs and opaque assistant replay metadata. A
`provider_state` history entry is a storage representation, **not a new MPP method
or automatic extraction from metadata**. General upstream-session creation,
replacement, reset, and uncertain-outcome reconciliation remain future MPP work.
A stored session ID never implies that a Provider can omit full selected context.

## Safety and limits

- Maximum record: 1,048,576 bytes before LF; snapshot: 67,108,864 bytes including
  delimiters; at most 100,000 entries. These are history-codec limits, independent
  of MPP framing. Larger histories need a separately designed paginated Store API.
- Integer fields use integer lexical forms (no decimal point/exponent) and retain
  full unsigned 64-bit precision. Opaque numbers use signed 64-bit negative integers,
  unsigned 64-bit nonnegative integers, or finite IEEE-754
  binary64 floats; overflowing integer tokens fail instead of being rounded into
  floats. Use strings for larger exact numbers. Float parsing preserves binary64
  round trips, not arbitrary-precision decimal lexemes. Implementations must not
  round revisions/timestamps through floating-point numbers.
- The strict decoder rejects duplicate JSON keys, including inside opaque data,
  rather than normalizing ambiguous input. JSON-valued payloads allow at most
  64 nested edges, excluding record wrappers; the JSON decoder also bounds total
  nesting. The encoder enforces the payload bound so caller-constructed values
  cannot produce unrestorable snapshots. Errors contain no record bodies or
  Provider values.
- Credentials, authorization URLs, and bearer-like continuation handles do not
  belong in ordinary history or portable exports. Use a separate credential store.
  Opaque values cannot be proven nonsecret by a schema: the producing Provider and
  host must apply that policy. Treat all conversation content as sensitive.
- Unknown format versions fail explicitly. There is no implicit migration, partial
  prefix recovery, or automatic replacement of IDs. Future migrations must preserve
  the original data and identities.

Fixtures and tests are under `conformance/history/`. They check the data contract,
not actual disk durability, exclusive ownership, live upstream resumption, or
end-to-end Agent Server restoration.
