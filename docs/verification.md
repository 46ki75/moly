# Verification

Verified on macOS arm64 with pinned Rust 1.98.0 and system Python 3.9.6. The repository
started empty. Earlier checkpoints reached 59, 82, 86, and 93 tests. The Provider
protocol work began by rerunning the **93-test / 3-doctest baseline successfully**.

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

This checkpoint implements the Model Provider extension boundary, **not all of
`REQUIREMENTS.md`**. Session Store protocol/client crate, tree persistence, exclusive
storage ownership, restoration, and Daemon behavior remain unimplemented. Streaming,
usage accounting, interactive authentication, reverse Provider host services, and
persistent Provider workers also remain outside this slice.

No live provider credentials or external inference services were exercised.
Windows/Linux native behavior, bundled-Provider cross-compilation, Windows ACLs and
console signals, arbitrary process-tree cleanup, terminal emulators, production
load, and durable recovery remain unverified. Native multi-platform CI is configured
but has not run here. JSON Schema references and typed fixtures are checked, not a
complete third-party JSON Schema validator. Python tests require an available Python
3 interpreter; its version is not pinned by mise. Unix signal smoke and startup
measurement remain manual checks; Python Provider conformance is in the normal gate.
