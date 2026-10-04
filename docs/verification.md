# Verification

Verified on macOS arm64 with pinned Rust 1.98.0 and system Python 3.9.6. The repository
started empty. Earlier checkpoints reached 59, 82, 86, and 93 tests. The Provider
protocol work began by rerunning the **93-test / 3-doctest baseline successfully**.

## Conversation history v1 / Agent Server naming checkpoint

- The unchanged baseline passed **263 tests and 6 SDK doctests**. A new contract
  asset check failed before the history schema/fixtures existed. The CLI naming
  regression failed before help identified the Agent Server as agentic-loop owner.
- `moly-protocol::history` adds independently versioned history DTOs and pure,
  bounded JSONL decoding/encoding, graph validation, and selected-ancestry queries.
  The separate `urn:moly:conversation-history:1` schema and golden snapshots preserve
  logical session IDs, branch selection, tool correlations, and opaque Provider
  replay/state. No runtime save/resume, Session Store, or new MPP operation is added.
- **35 new history tests** cover golden roundtrips, arbitrary physical order, exact
  branch exports, fork provenance, state clearing, malformed/duplicate/unknown
  fields, canonical IDs, ancestry/revision failures, branch-safe tool results,
  100,000-entry boundaries, UTF-8 byte limits, depth limits, and number precision.
  A draft negative test incorrectly rejected the permitted `state: null` reset;
  the parent isolated that case and corrected the test to match the contract.
- Retained regressions reproduced and fixed encoder output too deeply nested to
  decode, finite-float roundtrip drift, and overflowing integer tokens silently
  becoming floats. `serde_json` now enables `float_roundtrip`; no dependency version
  or project dependency edge changed. A schema check also caught and corrected the
  missing nonblank constraint on tool-result call IDs.
- The full `mise run check` passes **298 tests, zero skipped, and 6 SDK doctests**,
  including binary builds, rustfmt, and warnings-denied workspace Clippy. Formatting
  failed before applying rustfmt and is stable afterward. Warnings-denied rustdoc
  initially caught an unqualified history link; the corrected link passes.
- A scoped independent review confirmed the float issue and reported no other
  confirmed source findings. After the fix, the parent reran six reviewer probes,
  including the exact failing float, shape/depth/record bounds, u64 precision, and
  a small-forest tool-reference oracle. This was not a full-system security audit.
  Documentation/schema preparation workers reached their time limits; the parent
  completed integration, reviewed their artifacts, and ran the checks above.
- Documentation and CLI help/diagnostics now use **Agent Server**. `moly-server`,
  `ServerId`, `SERVER_VERSION`, roles `server` / `model_provider`, envelope v1,
  Agent Server v3, MPP v2, and historical schemas/traces are unchanged. All 31 CLI
  tests pass. The Unix signal smoke caught an old single-word diagnostic parser;
  after updating it, lazy spawn, inference, active/idle Ctrl-C, and Agent Server
  survival after CLI exit pass again with local mock HTTP.
- Linux ARM64 and Windows GNU all-targets cross-Clippy pass for protocol, both SDKs,
  CLI, and Agent Server. These are compilation checks, not native platform tests.
  Local Markdown paths/heading anchors were checked; external URLs were not fetched.

Schema structure/references and typed fixtures are checked, not a complete
third-party JSON Schema validator. These checks do not validate disk durability,
exclusive storage ownership, runtime restoration, migration, live upstream-session
resumption, OAuth/model access, or native Windows/Linux behavior. History v1 is a
limited data contract, not full persistence-requirements compliance.

## MPP SDK extraction / direct CLI checkpoint

- The unchanged `mise run --silent check` baseline passed **230 tests and 4 SDK
  doctests**. The new dependency-graph regression failed before extraction and
  passed afterward. CLI subprocess coverage reproduced `--direct` rejection
  before implementation and now passes.
- `moly-provider-client` is shared by Server and direct CLI hosts, with private
  framing/supervision and no executable imports, upstream HTTP, credential store,
  UI, or agent loop. Server leases, routing, and cancellation fences remain outside
  it. Envelope v1, MPP/Provider v2, Server v3, role `model_provider`, schema IDs and
  filenames, and historical fixtures are unchanged; only schema descriptions/title
  adopt the MPP name.
- The full `mise run check` passes **263 tests, zero skipped, and 6 SDK doctests**,
  including binary builds, rustfmt, and warnings-denied workspace Clippy. The SDK
  adds the nine shared framing checks and 16 independent Python-Provider process
  tests. They cover explicit environment/arguments, model/tool metadata, malformed
  replies, credential scope/rotation, inline callback cancellation, and actual
  child cleanup without a Server. Existing Server-path regressions still pass.
- Eight direct CLI subprocess tests cover lazy startup with an isolated CLI and
  no Server executable, login/status/logout, nonsecret registration permissions and
  conflicts, memory-only tokens, multi-turn metadata/IDs, `/new`, failed/cancelled
  context discard, queued input/EOF, child cleanup, no tools, and no host fallback.
  Most direct process cases are Unix-only; synthetic peers/local HTTP do not
  validate a live Codex service.
- A separate isolated CLI probe using the actual bundled Codex executable passed
  lazy startup, offline validation/signed-out status, owner-only nonsecret state,
  and clean exit without a Server executable. No login or inference was attempted.
- Linux ARM64 and Windows GNU all-targets cross-Clippy pass for CLI, both SDKs, and
  Server. A Windows-only unused test import was observed and is now conditionally
  imported. These are compilation/lint checks, not native runtime or bundled-Provider
  cross-compilation. Warnings-denied rustdoc for both SDKs/protocol also passes.
- A scoped read-only CLI reviewer found no actionable defects, reran all 31 CLI
  tests, and passed 110 extra loopback-only process-group SIGINT trials: 30 auth,
  30 independent-Provider model, and 50 bundled-Provider model cancellations.
  These macOS trials simulated terminal signal delivery; they were not actual
  terminal, native Windows/Linux, or live-service tests.
- A separate read-only SDK extraction review reached its 600-second time limit
  without a report. This is not a completed independent review or a security audit;
  the checks above are the reproducible verification evidence.
- The Unix unmanaged-Server/process-group signal smoke passes. The normal-mode
  first-prompt probe measured **2.08 / 2.48 / 4.94 ms** minimum/median/maximum across
  25 warmed samples on this host, not a portable performance guarantee.

Direct mode has CLI-memory credentials, no tools or Server sessions/replay, and no
Session Store access. Only nonsecret registration persists under the same Unix
policy. Live OAuth, model entitlement, and external inference remain unverified.

Earlier checkpoint results below describe their original scope; in particular,
Server-only credential storage and older dependency graphs are historical, not
claims about direct mode. Mock Provider/HTTP/OAuth tests do not validate a live
service. No new live-service access is authorized or reported for this extraction.

## Interactive authentication / Codex pilot (pre-MPP extraction)

- The unchanged `mise run --silent check` baseline passed **148 tests and 3 SDK
  doctests** before implementation.
- Server v3 / Provider v2 add connection-scoped auth operations, reverse URL
  interactions, and scoped opaque credential replacement. The new executable
  owns OAuth and Responses HTTP; no HTTP or OAuth dependency was added to Server
  or SDK. Historical Provider v1 schema and Server v1 trace remain intact.
- A deterministic regression reproduced cancellation arriving before the login
  command was accepted. Connection-scoped cancellation fences now reject that
  delayed command; the same regression passes after the fix.
- Independent review reproduced conflicting CLI registration overwrites,
  chunk-dependent SSE completion, rejected history after tool removal, and incorrect
  classification of headless interaction errors. Retained regressions failed before
  the fixes and passed afterward, including the CLI process ownership check.
  A fresh, scoped follow-up reviewer ran 13 focused tests and found no remaining
  actionable issues in those four fixes; this was not a whole-system security audit.
- A separate regression reproduced Serde accepting object-valued auth enums despite
  the string-only schema. Both auth operations and presentation outcomes now enforce
  strings; the same regression passes.
- Local signed-OIDC fixtures exercise PKCE/state/nonce, identity/scope failures,
  returning-account binding, refresh rotation and claims, revocation, and EOF
  cancellation. Responses fixtures exercise terminal SSE completion, failure/size
  limits, namespaced tools, and opaque replay across later turns.
- A standalone-process probe passed Provider v2 handshake, offline config validation,
  signed-out status, sanitized errors, and clean EOF exit without credentials or
  model access.
- The full gate passes **230 tests, zero skipped, and 4 SDK doctests**, including
  all executable builds, rustfmt, and warnings-denied workspace Clippy. Formatting
  failed before rustfmt, passed afterward, and was stable on the second pass.
- Warnings-denied SDK/protocol rustdoc, Linux ARM64 / Windows GNU all-targets
  cross-Clippy for CLI/SDK/Server, and Unix unmanaged-Server/signal smoke pass.
  Cross-Clippy is not native Linux/Windows execution or Provider cross-compilation.
  The CLI first-prompt probe measured **2.15 / 2.56 / 3.98 ms** minimum/median/maximum
  across 25 warmed samples on this host; this is not a portable performance guarantee.
- Public OpenAI [OIDC discovery](https://auth.openai.com/.well-known/openid-configuration)
  was read to confirm the documented issuer, authorization/token/revocation/JWKS
  endpoints, S256 support, and RS256 metadata. Reading metadata is not a live
  OAuth login, token exchange, or model-entitlement test.
- Credentials are Server-memory-only. CLI registration files currently require Unix
  owner-only permissions. Remote callbacks, device-code login, durable tokens,
  multi-account UX, and cross-Server refresh coordination are outside the pilot.

## Recoverable tool outcomes checkpoint

- The unchanged baseline passed **141 tests and 3 SDK doctests**.
- Three SDK/Server/Provider regressions and the Go CLI regression failed before
  implementation: missing-file recovery, mixed success/failure batches, the
  16-model-step failure bound, and Go tool-error continuation. Updated executor
  expectations also failed before the fix. All now pass.
- Known `read_file` failures now return sanitized `output.error` with the matching
  lease. Success payloads and all wire shapes/versions are unchanged; arbitrary RPC
  errors are not reclassified. Core uses its existing result-commit/continuation path.
- Conformance verifies originating call IDs, batch ordering, later-turn context,
  cancellation discarding partial context, stale-lease rejection for success/error
  outputs, invalid-workspace failure, and Client-hosted error-result versus RPC-error
  handling. Go mock HTTP retains session headers and opaque reasoning across recovery.
- The full gate passes **148 tests, zero skipped, and 3 SDK doctests**, including
  binary builds, formatting, and warnings-denied Clippy. Formatting failed before
  rustfmt, passes afterward, and is stable on a second pass. Startup and Unix
  subprocess/signal smoke pass. No running user Server was restarted or terminated.
- Permission-denied and generic I/O redaction are tested with synthetic I/O errors;
  this is not platform ACL validation. No live Go/model service or native Windows/Linux
  execution was exercised for this fix. Directory listing remains unimplemented.

## OpenCode Go checkpoint

- The existing 131-test / 3-doctest gate passed before changes. The initial Go
  `mise run check` passed **139 hermetic tests, zero skipped, and 3 SDK doctests**,
  including binary builds, formatting, and warnings-denied workspace Clippy.
- Four new Provider tests failed before implementation: missing session/User-Agent
  headers, ignored profile selection, missing-credential handling, and rejected
  reasoning replay. The real CLI/Server/Provider regression also failed on the
  missing session header. All now pass.
- The CLI resolves `MOLY_PROVIDER=opencode-go` to explicit options for the existing
  Provider executable. No new crate, runtime dependency, Server-specific Go logic,
  or protocol version change was needed.
- Mock HTTP verifies exact SessionId headers across fresh Provider invocations,
  tool steps, later turns, and `/new`; bearer auth and identifying User-Agent;
  opaque reasoning replay without rendering it; and no session header for ordinary
  OpenAI requests. Malformed reasoning, invalid profiles, missing credentials, and
  redirects remain rejected. CLI default/override resolution and lazy startup pass.
- The compatible adapter intentionally now accepts string/null `reasoning_content`
  as opaque replay data. Unsupported-field regressions retain coverage using
  `reasoning_details`; arbitrary structured reasoning is not newly accepted.
- Unix process/signal smoke and Linux ARM64 / Windows GNU all-targets cross-Clippy
  for CLI, SDK, and Server pass again. No live Go call or native Windows/Linux
  execution was performed. Go Responses and Anthropic Messages APIs are not
  implemented; the shared 60-second HTTP and 512-KiB response limits still apply.

### Review follow-up

- A fresh review baseline passed all 139 tests and 3 doctests. A separate actual
  Provider-process probe found that Serde accepted object-valued profiles such as
  `{"opencode-go": null}` despite the documented string option.
- The retained `non_string_profiles_are_rejected_by_validation_and_inference`
  regression failed before the fix. Both validation and inference now require a
  string when `profile` is supplied; omission still selects `openai`.
- The same subprocess probe now rejects non-string profiles with `invalid_config`
  and accepts both supported string profiles. The first fix passed 140 tests and
  3 doctests.
- A second fresh reviewer found that non-Unicode endpoint/model overrides silently
  selected defaults. An independent, non-networking Provider-process reproduction
  confirmed both fallbacks. The CLI now rejects malformed profile, endpoint,
  model, and API-key text with a typed, redacted configuration error.
- The retained Unix CLI subprocess regression failed before that fix. It covers
  both profiles and all four settings, unchanged configuration on rejection,
  recovery in the same REPL after another Client supplies valid configuration, and
  configured-Server precedence. Even on regression, a missing Provider executable
  prevents contacting a public default endpoint.
- The post-fix `mise run check` passes **141 tests, zero skipped, and 3 SDK
  doctests**, including formatting, Clippy, and binary builds. The Unix
  process/signal smoke and CLI/SDK/Server all-targets cross-Clippy for Linux ARM64
  and Windows GNU pass again. The new raw-environment runtime case is Unix-only;
  cross-Clippy is not a native Windows runtime check.
- The third fresh reviewer found no remaining actionable issues in the scoped Go
  changes and review fixes. It independently reran the full gate and subprocess
  probes for profile/credential validation, Go headers, reasoning replay, and CLI
  defaults. The loop closed with both confirmed defects fixed and regression-tested;
  live-service and native Windows/Linux runtime validation remain unperformed.

## Earlier Model Provider checkpoint

- `mise run check`: **131 hermetic tests, zero skipped, and 3 SDK doctests pass**.
  This includes workspace binary builds, rustfmt, and warnings-denied Clippy.
- The architecture regression failed before extraction and now checks five crates:
  `moly`, `moly-client`, `moly-server`, `moly-protocol`, `moly-provider-openai`.
  All executable crates are binary-only. Only the Provider depends on `reqwest`;
  the CLI uses the SDK, and other roles share only protocol/schema code.
- Six Python-provider integration tests exercise the actual Server through the
  public SDK. Audits verify the exact Server parent PID, explicitly supplied child
  environment, selected credential only, opaque non-HTTP configuration, normalized
  multi-turn/tool context, canonical events, and no implicit retries/fallback.
- Those tests also cover incompatible role/version, malformed/oversized responses,
  mismatched correlation, positional arrays, unsupported outcomes, redacted peer
  errors/stderr, duplicate tool IDs, and unadvertised tools. Entire invalid tool
  batches are rejected before any tool effect.
- Gated process tests verify cancellation terminates the Provider, replacement runs
  work, and Client disconnect leaves model work alive. Reattachment replays the
  same events without restarting inference. Bundled-Provider subprocess tests
  verify stdin EOF aborts both HTTP header waits and response-body reads, and
  malformed input exits even while stdin remains open.
- Bundled HTTP checks retain endpoint/authentication, redirect, timeout, response
  limit, malformed response, unsupported feature, and tool-call coverage. New
  replay tests check opaque metadata consistency and structured tool-result encoding.
  The Provider runs the same nine framing cases as Client/Server; Client/Server
  retain their full shared 22-case transport/trace suites.
- Review regressions failed before their fixes: stale CAS launched the selected
  program before rejecting the revision; Serde accepted positional arrays where
  the JSON contract requires objects; the bundled Provider emitted more than 32
  calls despite the protocol limit. Their retained checks now pass.
- Server protocol v2 intentionally changes resolved configuration. Envelope v1 and
  the original fixed-identity trace are retained; the historical trace uses its
  original configuration DTO, not a claim of live Server v1 support. Provider v1
  JSON Schema parsing/reference checks and typed payload round trips pass.
- Existing SDK, multi-client, reverse-tool, cancellation, replay, slow-consumer,
  configuration CAS, and eight CLI/REPL cases pass through the new Provider boundary.
- Warnings-denied rustdoc passes for SDK and protocol. CLI, SDK, and Server pass
  all-targets cross-Clippy for `aarch64-unknown-linux-gnu` and
  `x86_64-pc-windows-gnu`; these are compile checks, not native runtime checks.
- Unix smoke passes: lazy CLI/Server startup, local mock inference, active and idle
  process-group Ctrl-C, and Server survival. An isolated copied CLI without a
  Provider reports a recoverable `provider_unavailable`, preserves buffered local
  commands, and leaves its attached Server alive.
- Formatting failed before rustfmt and passed afterward. The first full gate caught
  duplicate inclusion of a test helper; the Server now includes that helper once.
  The final full gate passes. No commits, pushes, publication, or releases were made.

## First-prompt observation

`python3 conformance/startup.py target/debug/moly 25` measured **2.19 ms minimum /
2.53 ms median / 3.43 ms maximum** on this host after warmup. The preceding REPL
checkpoint measured 2.39 / 2.80 / 3.45 ms. No input was sent before the first prompt;
backend configuration and endpoint were deliberately unusable. Earlier cold checks
included 322 ms and 394 ms outliers. These observations include loader/OS effects;
they are not proof of improvement, a release benchmark, or a portable guarantee.

## Remaining scope and verification limits

These checkpoints cover the Provider extension boundary, shared MPP host SDK, and
limited direct CLI mode, and the separate history data contract, **not all of
`REQUIREMENTS.md`**. Session Store protocol/client
crate, tree persistence, exclusive storage ownership, restoration, and Daemon
behavior remain unimplemented.
Client-visible streaming, usage accounting, durable credentials, remote OAuth
callback routing, device-code login, and persistent Provider workers remain outside
this slice. Upstream Responses SSE is consumed internally by the new Provider.

No live provider credentials or external inference services were exercised.
Windows/Linux native behavior, bundled-Provider cross-compilation, Windows ACLs and
console signals, arbitrary process-tree cleanup, terminal emulators, production
load, and durable recovery remain unverified. Native multi-platform CI is configured
but has not run here. JSON Schema references and typed fixtures are checked, not a
complete third-party JSON Schema validator. Python tests require an available Python
3 interpreter; its version is not pinned by mise. Unix signal smoke and startup
measurement remain manual checks; Python Provider conformance is in the normal gate.
