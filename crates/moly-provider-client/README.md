# Moly Provider Protocol client SDK

Experimental host-side Rust SDK for **Moly Provider Protocol (MPP) v2**. Both the
Agent Server and the opt-in direct CLI use it to call standalone Providers. It is
separate from `moly-client`, which connects applications to an Agent Server.

Read [CONTRIBUTING.md](../../CONTRIBUTING.md) before changing code.

## Boundary

`ProviderClient` explicitly launches a resolved Provider command for each
`validate`, `authenticate`, or `step` call. It owns private JSONL framing,
initialization, correlation, operation-scoped reverse services, result validation,
error redaction, and child cleanup. The only shared project dependency is
`moly-protocol`, re-exported as `protocol`. No Provider implementation is linked.

Hosts own configuration discovery, conversation history, logical IDs, credential
storage/serialization, UI, and tool permissions/execution. Providers own OAuth and
upstream HTTP. Only the Agent Server owns the agentic loop; direct CLI hosting is
model-only chat/authentication and cannot access a Session Store. Session Store
integration is not implemented. This SDK has no agentic loop, browser launcher, automatic retry,
service discovery, process pool, implicit environment inheritance, or durable state.

The wire contract remains envelope v1 / MPP v2 with role `model_provider`;
[schemas and semantics](../../docs/model-provider-protocol.md) are language-neutral.
The Rust API is experimental and unpublished. No transport types are public.

## Validate and invoke a model

The caller supplies a Tokio runtime with process/I/O/timer support. Commands use
an absolute executable, literal arguments, and a complete explicitly supplied
environment. No shell or PATH lookup occurs. Provider stderr is discarded because
an independently implemented executable may put sensitive data there.

```no_run
use moly_provider_client::{ProviderClient, protocol::{
    ModelCallId, ProtocolError, RunId, SessionId, model::*,
}};

# async fn example(config: ProviderConfig, mut credential: Option<String>) -> Result<(), ProtocolError> {
let provider = ProviderClient;
provider.validate(&config).await?; // Offline; no credential or interaction access.
let request = ModelRequest {
    options: config.options.clone(),
    credential: None, // SDK snapshots the selected slot below, not this duplicate field.
    context: InferenceContext {
        session_id: SessionId::new(),
        run_id: RunId::new(),
        model_call_id: ModelCallId::new(),
        call_kind: CallKind::Primary,
    },
    messages: vec![ModelMessage::User { text: "Hello".into() }],
    tools: vec![],
};
match provider.step(&config, request, Some(&mut credential)).await? {
    ModelStep::Completed { text, metadata } => {
        // The host decides what to render and retains opaque metadata for replay.
        let _ = (text, metadata);
    }
    ModelStep::AwaitHostTools { .. } => unreachable!("no tools were advertised"),
}
# Ok(())
# }
```

A step returns one normalized outcome. The SDK validates the entire tool batch
before returning it, including advertised names, unique IDs, and object arguments.
It never executes tools. For Agent Server runs, the Agent Server must authorize
effects, retain assistant metadata/tool calls, append correlated tool results, and
explicitly request another step. No automatic replay follows uncertain failures.

## Authentication and scoped credentials

```no_run
use moly_provider_client::{Interaction, ProviderClient, protocol::{
    AuthAttemptId, ProtocolError, auth::*, model::ProviderConfig,
}};

# async fn login(config: ProviderConfig, credential: &mut Option<String>) -> Result<(), ProtocolError> {
let interaction = Interaction::new(|request| async move {
    // Supply your own UI policy. Never log the URL or execute it in a shell.
    let _ = request;
    Ok(InteractionOutcome::Declined) // Replace with the actual presentation outcome.
});
let status = ProviderClient.authenticate(
    &config,
    ProviderAuthRequest {
        attempt_id: AuthAttemptId::new(),
        operation: AuthOperation::Login,
        options: config.options.clone(),
        credential: None,
    },
    credential,
    Some(interaction),
).await?;
// Persist only nonsecret registration under your application's ownership policy.
// authenticated is local credential state, not proof of upstream model entitlement.
let _ = status;
# Ok(())
# }
```

`config.options` and the borrowed credential slot are authoritative: duplicate
request fields are replaced before transmission. Pass `None` as the step's slot
to grant no credential access; an empty selected slot is `Some(&mut None)`.
Only login can present interactions. Headless login has no terminal fallback.
Callbacks must not block the Tokio runtime: deadlines and cancellation require
cooperative async execution, not synchronous blocking UI/network work.
Status cannot replace credentials, and validation cannot read or replace them.
Login/logout and model refresh may replace only the selected slot, up to 64 KiB.
The SDK binds each interaction response to the operation's attempt ID.

**Hold exclusive access to the selected renewable credential for the entire call.**
Borrowing one slot enforces local exclusivity, but copying its value into separate
slots defeats that protection. The Agent Server holds its own per-reference lease; a
simple direct CLI serializes calls. Do not load the same rotating credential into
multiple independent hosts. Durable/zeroizing storage is not provided.

A replacement changes the slot before acknowledging the Provider. That change
survives a later error or dropped operation, including cancellation before the
acknowledgment reaches the child. Never restore an old token to simulate rollback.
`AuthStatus.registration` is untrusted nonsecret metadata; hosts must enforce their
own persistence and account-binding policy. An `Opened` response only acknowledges
presentation, not successful authentication.

## Lifetime, limits, and errors

- One fresh child per call, including validation. It is terminated after a result;
  there are no detached workers or transparent retries.
- Dropping the operation future kills its child and drops any pending callback
  future. Tokio provides best-effort reaping on cancellation; normal cleanup is
  bounded to one second. External OAuth/model effects cannot be rolled back.
- Handshake and validation each have three-second deadlines. Authentication has
  a total 300-second login / 30-second status/logout deadline; a model step has
  65 seconds. These include child startup and callbacks, but not time spent by
  the application waiting to acquire its credential lease.
- Protocol frames allow 1 MiB before LF. At most 64 sequential, strictly increasing
  reverse request IDs are accepted per operation; none during handshake. A model
  outcome can request at most 32 advertised hosted tools. Registration is limited
  to a 16-KiB object. Hosts still bound history and their own queues.
- `ProtocolError` supplies a stable category code and sanitized description. Match
  codes, not human-readable messages. Unknown Provider codes and messages are never
  reflected. Interaction callback errors are sanitized too: `provider_protocol`
  remains a protocol failure; other callback errors become `interaction_unavailable`.
- Configured executables are trusted native code, not sandboxed plugins. Arbitrary
  descendant process-tree cleanup and native cross-platform behavior are not
  guaranteed by this SDK.

Tests use the same root framing suite and independent Python Provider fixtures as
the Agent Server path. They do not validate a live OAuth or model service. See
[verification limits](../../docs/verification.md).
