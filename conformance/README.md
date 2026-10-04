# Long-lived behavior assets

Keep this directory outside implementation archives.

- `architecture/`: dependency graph checks for seven crates: protocol, two distinct
  SDKs, and four binary-only products. CLI uses Agent Client and MPP SDKs; Agent Server
  uses protocol and MPP SDK, never Agent Client SDK. Each SDK and bundled Provider
  uses only protocol among project crates; only Providers own upstream HTTP/OAuth.
- `protocol/`: framing and duplex contract tests for the Agent Client SDK and Agent Server
  private local-IPC transports; MPP framing is a separate host–Provider boundary.
- `sdk/`: external-consumer checks against the public Agent Client SDK facade.
- `model-provider/`: independent Python Moly Provider Protocol (MPP) v2 peer and
  real-process Agent Client SDK/Agent Server tests; these peers are also reused by MPP SDK tests.
- `mpp/`: external-consumer tests for the public MPP SDK, using those Python peers
  and additional independent framing/auth/lifetime scenarios without an Agent Server.
  SDK-private framing also runs the nine shared root framing checks.
- `auth/`: independent Python auth Provider and real-Agent Server Agent Client SDK tests
  for initiating-Client interaction routing, declined/unavailable presentation, private credential
  replacement, fresh-child status/model calls, and sanitized logs/config/history.
  Adversarial JSONL cases cover reused/wrong attempt IDs, unsafe URLs, cancellation,
  stale replies, and disconnect. Gated processes verify scoped refresh serialization,
  concurrent Client secret writes, immediate replacement commits, logout, and denied
  writes during validation/status or without a selected reference.
- `history/`: independent conversation-history v1 golden JSONL and public codec/
  semantic-validation tests. They preserve identity, ancestry, selected heads, and
  opaque replay/state; they do not test disk durability or runtime restoration.
- `schemas/`: language-neutral Component and history contracts, including preserved
  Provider v1. History format versions are independent of MPP/Agent Server versions.
- `traces/`: fixed-identity JSONL semantic fixtures, not production captures.
- `fixtures/`: deterministic provider and workspace inputs when shared fixtures
  become useful; current tests construct isolated temporary inputs inline.
- `state-transitions/`: multi-client, cancellation, replay, executor, and REPL
  conformance. Tool cases cover correlated recoverable outcomes, mixed batches,
  the model-step limit, fatal RPC/configuration faults, and cancellation/lease fences.
  REPL cases cover lazy local commands, multi-turn history, `/new`, errors, EOF,
  and idle disconnect with stdin open. A Go-profile case verifies conversation
  headers, missing-file recovery, tool/turn reasoning replay, `/new`, and CLI selection
  through actual CLI/Agent Server/Provider processes with local mock HTTP. The optional
  Unix Python smoke also checks process-group isolation and active/idle Ctrl-C.
- `support/`: subprocess test harness; builds are supplied externally, never started
  recursively by a test. Agent Server-path fixtures talk to the actual Agent Server executable;
  direct MPP fixtures host separate Providers, not Agent Server implementation code.
- `startup.py`: first-prompt latency measurement with an unusable backend endpoint.

`mise run --silent test` builds the workspace executables before running test adapters.
For standalone tests, first build the workspace binaries beside the test profile,
or set `MOLY_TEST_SERVER_BIN` and `MOLY_TEST_PROVIDER_BIN` to explicit executables.
Only test harnesses read these overrides. Python 3 is required for cross-language
Provider tests (`python3` on Unix, `python` on Windows). Missing executables or
interpreters are failures, not skips; no external Python packages are required.

## MPP extraction and direct-mode acceptance coverage

The gate includes independent SDK cases in `mpp/` and direct CLI subprocess cases
in `crates/moly/tests/direct.rs` with its independent Python fixture. See
[verification.md](../docs/verification.md) for completed checks and their limits.

Shared-host tests must preserve envelope v1, MPP/Provider v2, Agent Server v3, and
roles `server` / `model_provider`. Agent Server is the component name; older Server
terminology aliases it. `moly-server`, Rust symbols, wire identifiers, schema
IDs/filenames, and protocol versions remain unchanged for compatibility.
Agent Server-path audits must still show Agent Server-owned children and agentic loops,
credential leases, interactions routed only to the initiating Client, authoritative
session events, and model work surviving Client disconnect. Independent MPP SDK
fixtures must cover scoped credentials/replacement, inline callback cancellation,
bounded process cleanup, framing/tool limits, and sanitized errors without an Agent Server or an SDK-owned agentic loop.

Direct CLI subprocess fixtures must cover opt-in `--direct`, incompatibility with
`--connect`, lazy startup without any Agent Server, login/status/logout, memory-only
credentials, nonsecret registration permissions/conflicts, successful multi-turn
context and opaque metadata, `/new` preserving login, failed/cancelled turn discard,
queued input, and cancellation/child cleanup. They must verify `tools: []`, rejection
of returned tools before effects, and no agentic loop, Agent Server/Session Store
access, sessions/replay, multi-client authority, automatic host/model fallback, or
transparent retries. Session Store integration and CLI save/resume are not implemented.

## Retained contracts and fixture limits

The active workspace supplies thin test adapters. A rewrite may replace those
adapters, but must explicitly explain any changed invariant or incompatible trace.
The suites use Rust adapters and a Python Provider; they are not a separately
packaged language-neutral conformance runner. The original v1 JSONL trace remains
unchanged and is decoded using its historical configuration schema. Its Provider v1
schema and schema tests are preserved independently of current Agent Server v3/MPP v2.
MPP naming does not rename `model-provider-v1.json`, `model-provider-v2.json`, or
historical trace files.

Auth tests use Python's standard library and private local gates, not a live OAuth
service or the bundled Codex implementation. They validate host-service authority,
Agent Client SDK routing/lifetime behavior, and credential isolation, not OpenAI
registration, OIDC verification, refresh endpoints, revocation, or model entitlement. Provider audit
files intentionally contain fake sensitive sentinels and are temporary test evidence;
production host logs, config snapshots, and session history must not contain them.
Local OAuth/token/JWKS and Responses fixtures for the bundled Provider are separate
from these host tests; neither kind establishes live-service compatibility.
