# moly-provider-openai-codex

Private, binary-only Moly Provider Protocol (MPP) v2 implementation.
Read [CONTRIBUTING.md](../../CONTRIBUTING.md)
before changing code. Its only project dependency is `moly-protocol`; it does not
embed Codex, launch tools, manage accounts, persist secrets, or wrap a production
coding agent.

## Configuration and ownership

Options are deliberately limited to:

```json
{
  "model": "<explicit account-entitled model>",
  "host_id": "urn:uuid:<persisted UUIDv4>",
  "registration": null
}
```

After login, persist the returned nonsecret registration opaquely alongside the
host ID. Registration v1 contains `version`, `client_id`, `subject`, `issuer`, and
`host_id`. Issued clients bind account/workspace registrations; a subject or email
alone is not a workspace identifier. The Provider checks the selected mapping on
returning sign-in and token refresh.

The Client application owns the stable host ID and registration. The Provider's
host owns the selected memory-only credential slot and serializes its operations:
the Agent Server in normal mode, the CLI in explicit `moly --direct` mode. Only the
Agent Server owns the agentic loop; the Provider and direct CLI do not. Credentials are opaque
JSON v1 records containing the registration, tokens, scopes, and expiry information.
Login and refresh commit verified replacements through `host.credential.replace`
before success/inference. Status is local-only. Logout attempts revocation once,
then clears tokens even if revocation was not confirmed. Restarting the Agent Server or
exiting the direct CLI requires login again; retained registration is reusable.

Login uses documented direct SIWC dynamic registration, authorization code + S256
PKCE, state, nonce, and a `127.0.0.1` ephemeral `/auth/callback` listener. JWT
signatures use maintained `jsonwebtoken` cryptography, a validated discovery
issuer and JWKS, and an RS256/ES256 allowlist. Account binding, audience, authorized
party, expiry, issued-at/not-before, nonce, and optional access-token hash are
checked. Only grants with `openid`, `resource.invoke`, and
`chatgpt.tokens.use.direct` enable this inference-only pilot. Missing permission
is an explicit failure, not fallback to API-key billing. Authorization URLs omit
ID-token/email hints and are never logged.

## Limits and behavior

- JSONL envelope v1; positive monotonic direction-local IDs; one active operation;
  eight-frame input/output queues; at most 64 sequential host calls per operation.
  Frames are at most 1 MiB. Malformed or uncorrelated envelopes fail closed.
- EOF cancels active HTTP and callback work. Output flushing is bounded to one
  second. Login has a 295-second deadline; status/logout 27 seconds; model steps
  60 seconds, including refresh and credential commit.
- Networking disables redirects, ambient proxies, and retries. Production
  endpoints are fixed public OpenAI endpoints; loopback injection exists only in
  unit tests. Discovery/JWKS/token/credential records are capped at 64 KiB;
  callback headers at 16 KiB. Connect timeout is five seconds; individual OAuth
  requests have shorter bounded deadlines.
- Responses uses `store:false`, `stream:true`, full-context `input`, namespaced
  advertised function tools, and encrypted reasoning inclusion. No
  `previous_response_id`, native tools, background requests, or automatic retries.
  Only `response.completed` is success; failures, incomplete results, malformed
  streams, and interrupted streams never return partial success.
- SSE is capped at 2 MiB total and 512 KiB per event. Output/replay and normalized
  result are capped at 768 KiB. Replay preserves supported raw output items,
  including assistant phase, reasoning, and tool correlations; inconsistent
  metadata fails explicitly. Hosted batches are limited to 32 calls.
- Errors and stderr diagnostics are static and sanitized. Tokens are not a
  zeroizing vault; only the host's process-local slot retains them. No prompts,
  tool bodies, HTTP bodies, authorization URLs, or token values enter logs.

## Verification

```sh
cargo test --locked -p moly-provider-openai-codex
cargo clippy --locked -p moly-provider-openai-codex --all-targets -- -D warnings
cargo fmt -p moly-provider-openai-codex -- --check
```

Tests use loopback HTTP/SSE servers, a synthetic RSA fixture for locally signed
OIDC tokens, and the actual binary's STDIO. They cover claim/signature/state/scope
failures, returning identity, refresh rotation and terminal errors, revocation,
commit ordering, SSE completion/failure/tool continuation, replay, size limits,
and EOF cancellation. They do **not** validate live SIWC availability, model
entitlement, remote endpoint behavior, or actual browser presentation.

Sources:
[sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in.md),
[sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions.md),
[token reference](https://developers.openai.com/siwc/token-sharing-open-source/token-reference.md),
[OIDC](https://developers.openai.com/siwc/website.md),
[inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference.md),
[preview limits](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations.md),
[Responses schema](https://developers.openai.com/api/reference/resources/responses/methods/create.md).
