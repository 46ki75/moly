# Long-lived behavior assets

Keep this directory outside implementation archives.

- `architecture/`: five-crate dependency boundaries, binary-only products, and
  Provider-only HTTP dependencies.
- `protocol/`: framing and duplex contract tests, run against both private transports.
- `sdk/`: external-consumer checks against the public Client SDK facade.
- `model-provider/`: independent Python Provider and real-process SDK/Server tests.
- `schemas/`: language-neutral Component contracts; currently Model Provider v1.
- `traces/`: fixed-identity JSONL semantic fixtures, not production captures.
- `fixtures/`: deterministic provider and workspace inputs when shared fixtures
  become useful; current tests construct isolated temporary inputs inline.
- `state-transitions/`: multi-client, cancellation, replay, executor, and REPL
  conformance. Tool cases cover correlated recoverable outcomes, mixed batches,
  the model-step limit, fatal RPC/configuration faults, and cancellation/lease fences.
  REPL cases cover lazy local commands, multi-turn history, `/new`, errors, EOF,
  and idle disconnect with stdin open. A Go-profile case verifies conversation
  headers, missing-file recovery, tool/turn reasoning replay, `/new`, and CLI selection
  through actual CLI/Server/Provider processes with local mock HTTP. The optional
  Unix Python smoke also checks process-group isolation and active/idle Ctrl-C.
- `support/`: subprocess test harness; builds are supplied externally, never started
  recursively by a test. End-to-end fixtures talk to the actual Server executable.
- `startup.py`: first-prompt latency measurement with an unusable backend endpoint.

`mise run --silent test` builds all three executables before running test adapters.
For standalone tests, first build the workspace binaries beside the test profile,
or set `MOLY_TEST_SERVER_BIN` and `MOLY_TEST_PROVIDER_BIN` to explicit executables.
Only test harnesses read these overrides. Python 3 is required for cross-language
Provider tests (`python3` on Unix, `python` on Windows). Missing executables or
interpreters are failures, not skips; no external Python packages are required.

The active workspace supplies thin test adapters. A rewrite may replace those
adapters, but must explicitly explain any changed invariant or incompatible trace.
The suites use Rust adapters and a Python Provider; they are not a separately
packaged language-neutral conformance runner. The original v1 JSONL trace remains
unchanged and is decoded using its historical configuration schema.
