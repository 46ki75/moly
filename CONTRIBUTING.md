# Contributing

Humans and agents must read this guide before changing code.

## Rules

- Preserve [ownership boundaries](docs/architecture.md) and
  [wire semantics](docs/protocol.md), not implementation structure.
- Keep `moly`, `moly-server`, and `moly-provider-openai` binary-only with private
  implementation modules.
  The CLI depends on `moly-client`, whose only project dependency is `moly-protocol`.
  The Server depends only on protocol, never the SDK. No cross-role Rust dependencies,
  even for tests, or production source-inclusion shortcuts. Bundled Providers depend
  only on protocol, never Server internals; HTTP belongs in the Provider executable.
  Keep SDK transport private; process spawning, config discovery, and provider work
  do not belong in the SDK.
- Verify failures before fixing them; add deterministic, hermetic regression tests.
  No credentials or live providers in the ordinary test suite.
- Never log prompts, tool arguments/results, credentials, or provider bodies.
  Library crates emit tracing; executable roots install subscribers on stderr.
- Document public Rust items. Use typed recoverable errors; no `unwrap()`.
- Do not add plugins, traits, persistence, or distributed machinery speculatively.
- Record concrete limits in [architecture.md](docs/architecture.md). Archive only
  meaningful architectural generations. Preserve conformance assets at the root.
- Use `type[!]: summary` commits (`feat`, `fix`, `chore`, `test`, `refactor`, `docs`).
  Work on a branch and submit a PR; do not commit credentials or `.env` files.

## Tooling

Rustup owns the exact compiler in `rust-toolchain.toml`; this compiler is also the
initial, deliberately conservative MSRV. Mise pins auxiliary tools in `mise.toml`.

```sh
mise trust
mise install aqua:nextest-rs/nextest/cargo-nextest aqua:evilmartians/lefthook
rustup toolchain install --no-self-update
mise run --silent setup
mise run --silent fmt
mise run --silent check:quick
mise run --silent check
```

Rerun a failed task without `--silent` for full diagnostics. `check` is the same
quality gate used by CI: formatting, Clippy, nextest, and library doctests, with
locked dependencies. The test task first builds all three executables: SDK and CLI
fixtures launch `moly-server`, which alone spawns Providers. Tests never import
executable internals across roles. Python 3 is required for cross-language Provider
conformance (no external Python packages). For direct test invocations, build Server
and Provider first, or set test-only `MOLY_TEST_SERVER_BIN` and
`MOLY_TEST_PROVIDER_BIN` overrides (see [conformance](conformance/README.md)).

Formatting covers the entire active Rust workspace; review unrelated local edits
before formatting. Archives are excluded. Markdown, YAML,
TOML, JSON schemas, and Python fixtures/helpers receive manual review, not an automated formatting
guarantee.
Hooks check formatting without modifying/staging files; CI remains authoritative.

Compatibility is multidimensional: Rust source APIs, wire semantics, checkpoints,
and config/UX are separate contracts. Everything is experimental today; internal
crates are unpublished and freely replaceable. Update docs, fixtures, and tests
alongside any intentional semantic change.
