# Moly

An experimental Rust coding-agent runtime. Small deployments compose the same
Client, Server, provider, and executor boundaries intended for larger deployments.
Implementation is disposable; protocol semantics and conformance assets are not.

**Read [CONTRIBUTING.md](CONTRIBUTING.md) before making changes.** See
[architecture and limits](docs/architecture.md) and [Server protocol v2](docs/protocol.md).

## Current slice

- Lazy, multi-turn REPL CLI and a standalone, multi-client local-IPC Server.
- Bounded JSONL and duplex correlated RPC, including Client-hosted tools.
- Server-owned session actors, runs, model calls, cancellation, and event replay.
- Server-spawned, cross-language [Model Provider protocol](docs/model-provider-protocol.md),
  with bundled nonstreaming OpenAI-compatible and OpenCode Go profiles.
- Client-supplied resolved config, memory-only credentials, and a local `read_file` tool.
- No Session Store implementation, persistence, Daemon, scheduler, or remote gateway yet.

## Workspace

- `moly`: CLI only, using the [Client SDK](crates/moly-client/README.md).
- `moly-client`: experimental headless Rust SDK, with private client-side transport.
- `moly-server`: standalone Server; Core, Provider supervision, executors, and
  transports are private modules.
- `moly-provider-openai`: replaceable STDIO Provider executable; owns HTTP encoding
  and authentication, with no Server implementation dependency.
- `moly-protocol`: the only project crate shared by Client and Server sides;
  schema and semantics only.

The CLI does not link Server code. It starts or connects to the `moly-server`
executable and communicates exclusively through the protocol. The SDK neither
starts a Server nor resolves config; applications own those policies. It exposes
sessions, runs, cancellation, replay, config CAS, hosted tools, and typed errors.

## Try it

```sh
cargo build --locked --workspace
export MOLY_PROVIDER=openai
export MOLY_MODEL_ENDPOINT=https://api.openai.com/v1/chat/completions
export MOLY_MODEL=gpt-4.1-mini
# Supply MOLY_API_KEY securely in your environment; never commit it.
./target/debug/moly
```

Build/install all three executables side by side for default automatic spawning;
building only `moly` does not build the Server or Provider. The CLI resolves the
bundled Provider path after first input; `MOLY_PROVIDER_EXECUTABLE` can select another
executable. SDK callers can supply interpreter arguments and arbitrary Provider
options. `--connect` needs only the CLI when the existing Server is already configured;
otherwise the Client must supply a Provider executable path usable on the Server host.

### OpenCode Go

Use the same bundled executable with its explicit Go profile:

```sh
export MOLY_PROVIDER=opencode-go
export MOLY_MODEL_ENDPOINT=https://opencode.ai/zen/go/v1/chat/completions
export MOLY_MODEL=kimi-k2.6
# Supply your OpenCode Go key securely as MOLY_API_KEY.
./target/debug/moly
```

The endpoint and model above are the Go defaults; explicit environment overrides
win. Non-Unicode profile, endpoint, model, or API-key values are rejected without
printing their contents or treating them as absent. Use a fresh Server: attaching
to a configured Server does not read those settings or replace its configuration.
`MOLY_PROVIDER=openai` (or unset) retains the original defaults; unknown profiles
fail instead of falling back.

[Go requires](https://opencode.ai/docs/go#where-can-i-use-it) an identifying
User-Agent and `x-opencode-session`. The Provider sends `moly/<version>` and the
Server's conversation SessionId on every Go request, including tool follow-ups.
The ID remains stable across turns and changes with `/new`; no manual header is
needed. Only Go's **Chat Completions** models are supported—not its Responses or
Anthropic Messages models. See the [official endpoint table](https://opencode.ai/docs/go#endpoints)
for compatible model IDs. Support is mock-tested, not live-service validated.

### Startup and connections

The first prompt and local commands require no Server, provider, configuration,
or network. The first message lazily starts an **unmanaged Server** and prints its
identity, endpoint, and session. That Server intentionally survives CLI exit; stop
its printed diagnostic PID explicitly when finished.

To control process lifetime yourself, on Unix:

```sh
runtime_dir=$(mktemp -d)
./target/debug/moly-server --endpoint "$runtime_dir/server.sock"
# In another terminal:
./target/debug/moly --connect "<the-printed-endpoint>"
```

On Windows, use a unique named-pipe name such as `moly-dev-example` as the endpoint.
IPC types stay inside the SDK's and Server's private transport modules. Local peers
are trusted: use an owner-only Unix directory, and review Windows pipe access
controls before sharing a machine across trust boundaries. File-path confinement
is not an OS sandbox.

An attached CLI creates a new session and does not overwrite an existing Server
configuration. Protocol Clients can attach to an existing session by SessionId and
subscribe from the last observed sequence. The endpoint is not the Server identity.

## REPL

Run `moly --help` for usage. Messages share a conversation until `/new`:

| Input | Behavior |
| --- | --- |
| A message | Run inference and print the final assistant response |
| `/help` | Show commands without contacting a backend |
| `/new` | Detach from the old session; create a fresh one on the next message |
| `/quit` or `/exit` | Exit the CLI without stopping the Server |
| `//text` | Send `/text` as a message, not a command |
| Ctrl-C | Cancel the active run; exit when idle |
| EOF | Exit after processing queued input and finishing the active run |

Blank lines are ignored. Unknown slash commands are rejected locally. Input typed
while a run is active waits until it finishes; use Ctrl-C to request cancellation.
Configuration errors, rejected runs, and provider failures return to the prompt.
Connection loss or a command deadline exits with an explicit error, without
reconnecting or retrying an uncertain operation. Short SDK requests have a
10-second deadline; inference waits for a terminal event or cancellation.

This is a single-line REPL, not a full-screen TUI: no streaming output, multiline
editor, persistent input history, completion, or session browser. `/new` does not
delete the old Server-owned session.

## Verify

After the [tooling setup](CONTRIBUTING.md#tooling):

```sh
mise run --silent check
```

Tests require Python 3 for the independent Provider implementation, in addition to
the pinned Rust tooling. They use temporary workspaces and local mock HTTP, never
real API keys or external model services. Missing test executables/interpreters fail
rather than silently skipping. `conformance/` remains outside future archives.

Additional Python 3 checks after building:

```sh
python3 conformance/startup.py target/debug/moly
# Unix-only subprocess/signal regression:
python3 conformance/state-transitions/cli_unmanaged_unix.py
```

The startup benchmark records first-prompt latency, not an environment-independent
performance guarantee. See [verification results and limits](docs/verification.md).
