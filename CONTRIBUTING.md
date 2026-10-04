# Contributing

Humans and agents must read this guide before changing code.

## Rules

- Preserve [ownership boundaries](docs/architecture.md) and
  [wire semantics](docs/protocol.md), not implementation structure. Agent Server is
  the component name and owns the agentic loop; older Server terminology aliases
  it. Keep `moly-server`, Rust symbols, wire identifiers, schema IDs/filenames, and
  protocol versions unchanged for compatibility.
- Keep `moly`, `moly-server`, `moly-provider-openai`, and
  `moly-provider-openai-codex` binary-only with private implementation modules.
  The CLI uses `moly-client` for Agent Server access and `moly-provider-client` for
  explicit direct MPP access. The Agent Server uses protocol and the MPP host SDK,
  never the Agent Client SDK. Both SDKs depend only on `moly-protocol` among project crates. No
  executable implementation dependencies, even for tests, or production source-inclusion
  shortcuts. Bundled Providers depend only on protocol; upstream HTTP/OAuth belongs
  in Provider executables. SDK transports stay private. The MPP SDK may explicitly
  launch/supervise a resolved Provider command; the Agent Client SDK never spawns
  processes. Neither SDK discovers configuration, owns UI/credential persistence,
  or runs an agentic loop. Direct CLI mode remains model-only chat/authentication,
  without an agentic loop; Session Store access remains Agent Server-only and is
  not implemented yet.
- Verify failures before fixing them; add deterministic, hermetic regression tests.
  No credentials or live providers in the ordinary test suite. OAuth fixtures must
  use local authorization/token/JWKS services; passing them is not live-service validation.
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
locked dependencies. The test task first builds all four executables. Agent Server-path
fixtures launch `moly-server`, which owns its Providers; direct CLI/MPP SDK fixtures
host independent Provider processes without an Agent Server. Tests never import
executable internals across roles. Python 3 is required for cross-language Provider
conformance (no external Python packages). For direct test invocations, build Agent Server
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
