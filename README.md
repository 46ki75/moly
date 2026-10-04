# Moly

An experimental Rust coding-agent runtime. Small deployments compose the same
Client, Agent Server, Model Provider, and Executor boundaries intended for larger deployments.
Implementation is disposable; protocol semantics and conformance assets are not.

**Read [CONTRIBUTING.md](CONTRIBUTING.md) before making changes.** See
[architecture and limits](docs/architecture.md) and [Agent Server protocol v3](docs/protocol.md).

**Agent Server** owns the agentic loop and authoritative live session/run state.
This is the component name, not a binary or protocol rename: `moly-server`,
`ServerId`, `SERVER_VERSION`, wire role `server`, method strings, schema IDs/filenames,
and protocol versions remain unchanged for compatibility. Older **Server**
terminology aliases Agent Server.

## Current slice

- Lazy, multi-turn REPL CLI and a standalone, multi-client local-IPC Agent Server.
- Bounded JSONL and duplex correlated RPC, including Client-hosted tools.
- Agent Server-owned agentic loops, session actors, runs, model calls, cancellation,
  and event replay.
- Cross-language [Moly Provider Protocol (MPP)](docs/model-provider-protocol.md),
  hosted by the Agent Server or opt-in direct CLI, with bundled OpenAI-compatible/OpenCode Go
  profiles and a separate ChatGPT OAuth pilot.
- `moly --direct`: authentication and multi-turn model-only chat without an Agent
  Server; no agentic loop, tools, Agent Server sessions/replay, or multi-client authority.
- Normal Agent Server mode: Client-supplied resolved config, memory-only credentials, and
  a local `read_file` tool.
- Independent [conversation history v1](docs/conversation-history.md) schema and
  pure JSONL snapshot validation; stable session IDs, branches, and opaque replay data.
- No Session Store, runtime save/resume, Daemon, scheduler, or remote gateway yet.

## Workspace

- `moly`: CLI only; uses the [Agent Client SDK](crates/moly-client/README.md) for
  Agent Server access and the MPP host SDK for explicit direct access.
- `moly-client`: experimental headless Agent Client SDK, with private Agent Server transport
  and no process spawning.
- [`moly-provider-client`](crates/moly-provider-client/README.md): common experimental
  MPP host SDK; explicit resolved Provider commands, child supervision,
  authentication/step operations, and private STDIO transport.
- `moly-server`: standalone Agent Server; agentic-loop Core, credential authority,
  MPP host adapter, executors, and Agent Server transport are private modules.
- `moly-provider-openai`: replaceable STDIO Provider executable; owns Chat Completions
  HTTP encoding and bearer authentication, with no Agent Server implementation dependency.
- `moly-provider-openai-codex`: independent ChatGPT OAuth/Responses Provider;
  authentication interactions use generic MPP host services, never the Client terminal.
- `moly-protocol`: shared Component/history schemas and pure validation; no transport
  or runtime ownership.

All four executables remain binary-only; none links another executable's implementation.
Both SDKs depend only on `moly-protocol` among project crates. The Agent Server uses protocol
and the MPP SDK, never the Agent Client SDK; bundled Providers use only protocol.
Upstream HTTP/OAuth remains in Provider executables.

In normal mode the CLI starts or connects to `moly-server` exclusively through its
public protocol. `moly-client` exposes sessions, runs, cancellation, replay, config
CAS, hosted tools, and typed errors; it neither starts processes nor resolves config.
`moly-provider-client` instead launches a resolved Provider for each operation and
has no config discovery, UI, credential persistence, history, or agent/tool loop.
The host owns credential slots and serializes renewable credentials for each whole
operation; the SDK does not own a secret store.

## Try it

```sh
cargo build --locked --workspace
export MOLY_PROVIDER=openai
export MOLY_MODEL_ENDPOINT=https://api.openai.com/v1/chat/completions
export MOLY_MODEL=gpt-4.1-mini
# Supply MOLY_API_KEY securely in your environment; never commit it.
./target/debug/moly
```

For normal mode, build/install the CLI, Agent Server, and selected Provider side by
side for automatic spawning; building only `moly` does not build the Agent Server or
Provider. Direct mode requires only the CLI and selected Provider, not an Agent Server executable.
The CLI resolves the bundled Provider path after first input; `MOLY_PROVIDER_EXECUTABLE` can select another
executable. SDK callers can supply interpreter arguments and arbitrary Provider
options. `--connect` needs only the CLI when the existing Agent Server is already
configured; otherwise the Client must supply a Provider executable path usable on
the Agent Server host.

### Direct mode

With the same Provider settings, explicitly opt in:

```sh
./target/debug/moly --direct
```

`--direct` and `--connect` are mutually exclusive. The default Agent Server path is
unchanged; there is no automatic fallback between modes or to another Provider/model.
Direct mode never starts or contacts an Agent Server and has no agentic loop. The first prompt and local commands
remain lazy; authentication and multi-turn model-only chat use the common MPP SDK.

Initial direct mode advertises no tools, rejects returned tool requests, and performs
no tool effects. Conversation context and opaque replay metadata stay in the CLI;
only successful turns are retained. Local conversation IDs do not create Agent Server
sessions, event replay, saved-session access, Session Store access, or multi-client
continuity. `/new` resets conversation/SessionId, not login. Credentials live only
in CLI memory and vanish at exit; only nonsecret host identity/registration persists
under the same Unix owner-only permission and conflict-protection policy.
See [verification results and limits](docs/verification.md) for the hermetic checks.

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
printing their contents or treating them as absent. Use a fresh Agent Server: attaching
to a configured Agent Server does not read those settings or replace its configuration.
`MOLY_PROVIDER=openai` (or unset) retains the original defaults; unknown profiles
fail instead of falling back.

[Go requires](https://opencode.ai/docs/go#where-can-i-use-it) an identifying
User-Agent and `x-opencode-session`. The Provider sends `moly/<version>` and the
host's conversation SessionId on every Go request: Agent Server-issued in normal mode,
CLI-local in direct mode. Normal mode includes tool follow-ups; direct mode has no
tools. The ID remains stable across turns and changes with `/new`; no manual header
is needed. Only Go's **Chat Completions** models are supported—not its Responses or
Anthropic Messages models. See the [official endpoint table](https://opencode.ai/docs/go#endpoints)
for compatible model IDs. Support is mock-tested, not live-service validated.

### ChatGPT OAuth pilot

The separate `openai-codex` Provider uses OpenAI's documented
[Sign in with ChatGPT for open-source/local apps](https://developers.openai.com/siwc/token-sharing-open-source.md)
and public Responses API. It does **not** embed the Codex CLI agent, reuse its login
file/client ID, or use private ChatGPT backend endpoints.

```sh
export MOLY_PROVIDER=openai-codex
export MOLY_MODEL="<model-available-to-your-account>"
export MOLY_AUTH_STATE_FILE="$HOME/.moly-codex-registration.json"
./target/debug/moly
# Enter /login, then open the displayed authorization URL in your browser.
```

Use a fresh Agent Server or explicitly configure an existing one: attaching to a
configured Agent Server never overwrites its profile from environment variables. Model selection is
explicit; login is not proof of model entitlement. `/auth` inspects local authentication
status, and `/logout` clears the credential and attempts upstream revocation.

For direct login/chat, use the same settings with `./target/debug/moly --direct`
and enter `/login`. The pilot requires the browser and Provider host on the same
machine (Agent Server in normal mode, CLI in direct mode). CLI-managed state currently
requires Unix owner-only file permissions and protects against conflicting registration
updates. Stable host identity and nonsecret registration metadata are saved in the
explicit state file; tokens stay only in Agent Server memory until Agent Server restart, or in
direct CLI memory until CLI exit. Do not put API keys or tokens in that file. A
Provider process can exit without losing its host-held credential.
No device-code flow, remote callback routing, durable tokens, or Client-visible
streaming is implemented. See [authentication ownership and limits](docs/authentication.md).
Local OAuth/SSE fixtures are not live OpenAI validation.

### Startup and connections

The first prompt and local commands require no Agent Server, provider, configuration,
or network. In normal mode the first backend operation (a message or authentication
command) lazily starts an **unmanaged Agent Server**. The CLI prints its diagnostic PID;
the first message also prints its identity, endpoint, and session. That Agent Server
intentionally survives CLI exit; stop its printed diagnostic PID explicitly when
finished.

To control process lifetime yourself, on Unix:

```sh
runtime_dir=$(mktemp -d)
./target/debug/moly-server --endpoint "$runtime_dir/server.sock"
# In another terminal:
./target/debug/moly --connect "<the-printed-endpoint>"
```

On Windows, use a unique named-pipe name such as `moly-dev-example` as the endpoint.
IPC types stay inside the Agent Client SDK's and Agent Server's private transport modules.
Local peers are trusted: use an owner-only Unix directory, and review Windows pipe access
controls before sharing a machine across trust boundaries. File-path confinement
is not an OS sandbox.

An attached CLI creates a new session and does not overwrite an existing Agent Server
configuration. Protocol Clients can attach to an existing session by SessionId and
subscribe from the last observed sequence. The endpoint is not the Agent Server identity.

## REPL

Run `moly --help` for usage. Messages share a conversation until `/new`:

| Input | Behavior |
| --- | --- |
| A message | Run inference and print the final assistant response |
| `/help` | Show commands without contacting a backend |
| `/new` | Normal: detach from the old Agent Server session; direct: reset local conversation/SessionId. Login is unchanged |
| `/login` | Authenticate through the configured Provider; initializes the backend lazily |
| `/auth` | Inspect local Provider authentication status |
| `/logout` | Clear the selected credential and attempt upstream sign-out |
| `/quit` or `/exit` | Exit; normal Agent Server survives, direct credentials/context are lost |
| `//text` | Send `/text` as a message, not a command |
| Ctrl-C | Cancel the active run or login; exit when idle |
| EOF | Exit after processing queued input and finishing the active run |

Blank lines are ignored. Unknown slash commands are rejected locally. Input typed
while a run is active waits until it finishes; use Ctrl-C to request cancellation.
Configuration errors, rejected runs, and provider failures return to the prompt.
In normal mode connection loss or a command deadline exits with an explicit error,
without reconnecting or retrying an uncertain operation. Short Agent Server commands have
a 10-second deadline; inference waits for a terminal event or cancellation. In
direct mode Ctrl-C drops the active MPP SDK operation and kills its Provider child;
there is no Agent Server event stream. Login uses a separate 300-second deadline in both
modes; status/logout use 30 seconds and model steps 65 seconds. Neither mode retries
uncertain operations. During authentication, EOF or `/quit` cancels and exits;
other typed input is ignored.

In normal Agent Server mode, known `read_file` failures (for example, a missing file)
are returned to the model as [correlated tool results](docs/protocol.md#tool-execution-outcomes), allowing it
to correct its request or ask for help. Runtime faults still fail the run. Tools
are not automatically retried, and only `read_file` is bundled—no directory listing.

This is a single-line REPL, not a full-screen TUI: no streaming output, multiline
editor, persistent input history, completion, or session browser. `/new` does not
delete the old Agent Server-owned session in normal mode; direct mode has no saved
or Agent Server-owned session to retain.

## Verify

After the [tooling setup](CONTRIBUTING.md#tooling):

```sh
mise run --silent check
```

The current gate passes **298 tests and 6 SDK doctests**, including history-contract,
MPP, and direct-mode fixtures. These checks do not validate live OAuth or model services.

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
