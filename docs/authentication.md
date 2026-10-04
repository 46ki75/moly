# Interactive authentication pilot

Authentication is a protocol workflow, not a hosted model tool or a terminal owned
by a Provider. A Moly Provider Protocol (MPP) host may be the Agent Server or explicit
`moly --direct` CLI. Each owns only its own Provider children; Agent Server-routed
operations remain Agent Server-owned. Client–Agent Server semantics are v3; MPP (Provider)
semantics remain v2 with role `model_provider`; the common envelope remains v1.
See the unchanged [Provider schema](../conformance/schemas/model-provider-v2.json),
[Agent Server protocol](protocol.md), and [MPP specification](model-provider-protocol.md).

## Ownership and messages

```text
Normal Agent Server mode:
Client → Agent Server → Provider: explicit authentication operation
Provider → Agent Server → initiating Client: present authorization URL
Client → Agent Server → Provider: presentation outcome
Browser → Provider loopback callback: authorization result
Provider → Agent Server → Client: nonsecret authentication status

Explicit direct CLI mode (no Agent Server):
CLI → Provider: explicit authentication operation through MPP SDK
Provider → CLI: authorization URL / scoped credential replacement / status
CLI → Provider: presentation outcome through MPP SDK
Browser → Provider loopback callback: authorization result
```

The Provider owns OAuth, upstream HTTP, identity validation, token exchange,
refresh, and revocation. In normal mode the Agent Server routes generic interactions and
leases an opaque credential reference; the Client presents the interaction. In
direct mode the CLI presents it and lends its own local credential slot to the MPP
SDK. The CLI persists only nonsecret host identity/registration in either mode.
No OAuth mechanics belong in Agent Server Core, the CLI, or either SDK. A browser
acknowledgment is not authentication success.

Agent Server-protocol Client operations (normal mode only):

| Method | Parameters | Result |
| --- | --- | --- |
| `provider.auth` | `AuthCommand`: fresh `attempt_id`, `operation`, `config_revision` | `AuthStatus` |
| `auth.cancel` | `AuthCancel`: `attempt_id` | null |
| `interaction.request` (reverse) | `InteractionRequest`: `attempt_id`, `url` | `InteractionResponse`: matching ID, `outcome` |

Operations are `login`, `status`, and `logout`. The Agent Server rejects a stale config
revision before launching the Provider, then pins that configuration for the
operation. A valid `secret_ref` is required, but its slot may initially be empty.
Validation remains offline and credential-free; it never prompts or logs in.
`status` examines local credential state, not model access or upstream entitlement.

In direct mode `/login`, `/auth`, and `/logout` instead call
`moly-provider-client::ProviderClient::authenticate` with the resolved Provider
config, selected credential slot, and an inline `Interaction` callback. There is
no Agent Server config revision, secret reference, connection registry, or `auth.cancel`
RPC. The CLI generates a fresh attempt ID; dropping the operation cancels it and
its callback. The MPP SDK supplies that ID in the reverse presentation response.
The Agent Client SDK, `moly-client`, remains the separate Agent Server-access API and
never spawns processes.

Provider operations and host services (both hosts):

| Method | Parameters | Result |
| --- | --- | --- |
| `provider.auth` | `ProviderAuthRequest`: attempt ID, operation, options, private credential | `AuthStatus` |
| `host.interact` (reverse) | `InteractionRequest` | `InteractionResponse` |
| `host.credential.replace` (reverse) | `CredentialReplace`: required nullable `credential` | null |

The Provider receives only the selected credential record. Replacement requests
have no key selector: they can replace or clear only that operation's slot.
A nonnull record is limited to 64 KiB of UTF-8. Hosts must serialize all use of a
renewable credential for the entire operation, including interaction. In Agent Server
mode this includes `secret.put` and inference sharing a reference; other references
remain independent. Direct mode serializes its selected CLI-local slot and makes
no `secret.put` call. The MPP SDK has no secret store; it reads the borrowed slot
and commits replacement immediately, so later children do not receive stale tokens.
Status, validation, and handshake have no credential-write authority. Model steps
may update credentials for refresh but cannot request user interaction; a required
login is an explicit error, not an implicit inference retry.

`AuthStatus` echoes `attempt_id` and contains `authenticated`, optional nonsecret
`registration`, and optional `revocation_confirmed`. Registration is a Provider-defined
object limited to 16 KiB of serialized UTF-8, never a token or credential record.
Clients may persist it for returning sign-in. Only the Provider interprets it.
A false revocation result means local sign-out is not proof of remote revocation.

## Routing, cancellation, and limits

- Interaction URLs must use HTTPS, contain no control characters, and fit in 8192
  UTF-8 bytes. They are presentation data, not shell commands. Treat URLs as
  sensitive: do not place them in tracing or conversation history.
- `opened`, `declined`, and `unavailable` describe presentation only. The Provider
  independently validates the OAuth callback and tokens before replacing credentials.
- In Agent Server mode, interactions go only to the initiating connection, never broadcast
  to session subscribers or silently reassigned to another Client. In direct mode,
  only the calling CLI's inline interaction callback receives them.
- Each Agent Server connection accepts at most eight concurrent auth operations and 1024
  distinct attempt IDs over its lifetime, including cancellation identities.
  IDs cannot be reused. An unknown/completed attempt returns `not_active` on cancellation. An
  unknown ID is also fenced on that connection: a delayed original command cannot
  start afterward. Another connection cannot cancel or fence the owner's operation.
- Login has a 300-second total deadline, including Agent Server credential contention and
  process startup. Status/logout have 30 seconds. Model steps have a 65-second total
  deadline, including Agent Server credential contention. These are separate from short Agent Client
  SDK/CLI commands; direct operations use the same auth/model deadlines without a
  Agent Server credential lease.
- In Agent Server mode cancel, Client disconnect, or Agent Server shutdown aborts
  that auth operation and kills its Agent Server-owned child, not unrelated sessions/runs. Direct
  CLI Ctrl-C drops its MPP SDK operation; EOF or quit during auth cancels and exits.
  The callback is inline and is dropped on cancellation, not left in a spawned
  registry. The Provider must abort HTTP/callback work on STDIO EOF. Late replies
  cannot complete a replacement operation.
- Each Provider operation has a fresh initialized child. It may issue up to 64
  sequential reverse host requests, with strictly increasing positive direction-local
  IDs. No reverse requests are accepted during handshake. Unexpected events,
  correlation mismatches, and malformed frames fail the connection.
- A committed credential update survives cancellation of the enclosing operation.
  No rollback can undo token rotation or revocation already performed upstream.
  Lost responses leave outcomes uncertain: inspect status; do not blindly replay login,
  token exchange, refresh, or model inference.

## Storage and deployment scope

The pilot is single-machine, with trusted local peers. CLI-managed registration
files currently require Unix owner-only permissions; Windows registration-file
policy has not been implemented. The Provider listens for an OAuth callback on the
Provider host's machine (Agent Server in normal mode, CLI in direct mode); a browser on
another machine cannot use that loopback address. Remote browser routing and
device-code authentication are not implemented. An interactive Client is required
for login; headless Clients receive an explicit unavailable result rather than
causing a terminal prompt in the Provider.

Credentials live only in the hosting application's memory, are not zeroized, and
disappear on Agent Server restart in normal mode or CLI exit in direct mode. A Provider
child exiting does not clear its host's slot, nor does `/new`. Credentials never
enter configuration snapshots, conversation history, or ordinary logs. Stable host
identity and nonsecret account registration survive restart through Client-owned
configuration. Both CLI modes use the same Unix owner-only registration permissions
and conflict protections; direct mode does not persist tokens in that file.
Durable token/keychain storage, multi-account UX, and cross-host coordination of
shared refresh credentials are outside this pilot. Do not load the same renewable
credential into independent Agent Servers or direct CLI hosts.

The Agent Server owns the agentic loop. Direct mode is authentication plus multi-turn
model-only chat: no agentic loop, advertised tools, tool effects, Agent Server
sessions/replay, saved-session access, or multi-client authority. The MPP SDK has
no agentic loop, and the Agent Client SDK never spawns processes. Session Store
access remains Agent Server-only; its integration and CLI save/resume are not
implemented. `--direct` is incompatible with `--connect`, and a failed Agent Server
operation never falls back to direct hosting.

## OpenAI integration contract

The standalone `moly-provider-openai-codex` targets the documented **Sign in with
ChatGPT for open-source/local apps**, not the Codex CLI agent loop. It must use the
app's own dynamic registration and public Responses API, not Codex's client ID,
`~/.codex/auth.json`, or private `backend-api` endpoints. The Agent Server retains authority
over its agent runs and tool execution; direct chat retains only local model context
and performs no tool effects. Account/model availability is not established by login.

Primary sources for the implementation:

- [Registration, PKCE, callback, OIDC, and granted scopes](https://developers.openai.com/siwc/token-sharing-open-source/sign-in.md).
- [Stable host identity](https://developers.openai.com/siwc/token-sharing-open-source.md).
- [Refresh serialization, rotating tokens, and logout](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions.md).
- [Responses HTTP/SSE and terminal completion](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference.md).
- [Preview restrictions, full context, and supported tools](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations.md).

For this route, HTTP inference requires `store: false`, `stream: true`, and explicit
context on each request. The Provider may consume SSE internally and return the
existing completed/tool outcomes; Client-visible streaming is a separate feature.
Interrupted/failed/incomplete streams are not successful completions. No automatic
fallback to API-key billing or retries of uncertain model calls are permitted.

## Running the Provider executable

The executable is a protocol peer, not an interactive login/chat CLI:

```sh
cargo build --locked -p moly-provider-openai-codex
./target/debug/moly-provider-openai-codex
```

Keep stdin open and exchange MPP v2 JSONL, starting with `initialize`.
Your host must handle `host.interact` and `host.credential.replace`; the executable
never owns terminal UI or durable credentials. Stdout is reserved for protocol
frames. Launching this executable is not the interactive `moly --direct` mode.
For interactive use, follow the [CLI setup](../README.md#chatgpt-oauth-pilot), use
`moly` (normal Agent Server path) or opt in with `moly --direct`, and enter `/login`.

## Verification boundary

Tests use independent Python Provider processes, local OAuth/token/JWKS fixtures,
and local Responses streams. They cover interaction routing/cancellation, credential
scoping and refresh serialization, callback/token validation, hosted-tool continuation,
and independent MPP SDK/direct CLI hosting without external services or real
credentials. See [verification.md](verification.md) for exact scope and platform limits.
Passing mock fixtures does not validate OpenAI admission, model entitlement,
production OAuth redirects, or live service compatibility. A live smoke test requires
separate explicit authorization and must be reported independently.
