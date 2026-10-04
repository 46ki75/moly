# Moly Requirements

## 1. Purpose

This document specifies required behavior and architectural constraints for Moly, a modular coding-agent system. It is not an implementation inventory, progress report, or compliance assessment.

Moly SHOULD be usable as a small local application while preserving clean boundaries for additional capabilities. Internal architecture MAY be rewritten when practical limits are discovered.

Initial-version scope and delivery sequencing are specified in sections 30 and 31. Implementation status, deviations, and verification results belong in separate documents.

Normative terms **MUST**, **SHOULD**, and **MAY** are used intentionally.

---

# 2. Primary Components

## 2.1 Components and implementations

A **Component** is a logical part that communicates with other Components through a versioned, schema-defined protocol.

The architecture MUST distinguish these primary Component roles:

- **Client**
- **Agent Server**
- **Daemon**
- **Model Provider**
- **Session Store**

**Agent Server** is the formal component name for the owner of the agentic loop.
Older **Server** terminology aliases Agent Server. This naming change MUST NOT
rename the `moly-server` executable/crate, Rust symbols such as `ServerId` and
`SERVER_VERSION`, wire role `server`, method strings, schema IDs/filenames, or
protocol versions; those identifiers remain unchanged for compatibility.

These roles MUST remain logically distinct even when packaged or distributed together. Component boundaries MUST NOT depend on a particular programming language, crate layout, transport, or codec. Transport and codec implementations carry protocol messages; that alone does not make them Components.

An **implementation** is concrete code implementing a Component's contract. An **instance** is a running realization of that implementation. For example, filesystem and DynamoDB Session Stores are different implementations; multiple processes may run the same implementation. An implementation, a running instance, and the storage it accesses MUST remain distinct concepts.

## 2.2 Operations, processes, and handshakes

- **Originate an operation**: issue the request that begins a logical workflow.
- **Spawn a process**: create an operating-system process.
- **Initiate a handshake**: send the first handshake message on a particular protocol connection.
- **Supervise a process**: monitor its lifecycle and control termination or restart policy.

Operation origination, process spawning, and handshake initiation MUST remain distinct. An operation MAY use an already-established connection. Who originated an operation or spawned a process MUST NOT by itself determine which peer sends the first handshake message; each protocol MUST define its handshake behavior.

Process supervision MUST NOT by itself confer authority over live conversation state.

---

# 3. Client Requirements

## 3.1 Client responsibilities

A Client MAY:

- create and attach to sessions
- list, open, and resume saved conversations
- browse history, select branches, and fork conversations
- submit user messages
- start runs
- cancel runs
- subscribe to Agent Server events
- provide tools to the Agent Server
- provide resolved configuration to the Agent Server
- participate in user-facing authentication or interaction flows

The Client MUST interact with the Agent Server through the public Agent Server protocol rather than Agent Server implementation APIs. Explicit direct Provider access as specified in section 3.3 is a separate workflow, not a bypass for Agent Server operations.

The user-facing CLI MUST NOT depend directly on Agent Server implementation internals.

---

## 3.2 CLI startup

The CLI SHOULD optimize aggressively for time to first interactive render.

Displaying the initial UI and accepting user input MUST NOT require:

- a running Agent Server
- a running Daemon
- a model provider connection
- provider authentication
- network access
- project scanning
- model discovery
- history storage connectivity or authentication
- history enumeration or restoration

Agent Server, Model Provider, and Session Store initialization SHOULD be lazy.

A typical startup flow SHOULD be:

    CLI process starts
        ↓
    initialize minimal UI state
        ↓
    render interactive prompt
        ↓
    accept input
        ↓
    first operation requiring the Agent Server
        ↓
    resolve configuration
    locate or spawn Agent Server
    connect to Agent Server
        ↓
    submit operation

The Client MAY perform speculative background prewarming after first render, but failure of such prewarming MUST NOT prevent normal interaction.

---

## 3.3 Explicit direct CLI mode

The CLI MAY host a Provider directly through the **Moly Provider Protocol (MPP)** for authentication and multi-turn, model-only chat. This is a host responsibility, not a new Component role or a Client-owned Agent Server runtime.

- Direct mode MUST be explicitly selected with `moly --direct`. It MUST be incompatible with `--connect`. The normal Agent Server path MUST remain the default, with no automatic fallback between modes or Providers.
- Direct mode MUST NOT spawn or contact an Agent Server. Provider initialization MUST remain lazy under section 3.2.
- The direct CLI MUST own its Provider children, resolved Provider configuration, interaction UI, in-process credential slot, and local conversation context. It MUST preserve supported opaque replay metadata across successful turns and discard failed or cancelled turn working context.
- The initial direct mode MUST advertise no tools, reject returned tool requests, and perform no tool effects. It MUST NOT independently execute an agentic loop.
- Local context and correlation identities MUST NOT confer Agent Server session ownership, canonical event replay, saved-session access, or multi-client continuity. `/new` MUST reset the local conversation identity/context without clearing authentication.
- Credentials MUST remain in CLI process memory and be lost at exit. Persisted host identity and registration metadata MUST be nonsecret and follow the same Unix owner-only permission and conflict-protection policy as the Agent Server-path CLI.
- Direct mode MUST NOT communicate with a Session Store, mutate Agent Server-owned history, take over an Agent Server run, or weaken the Agent Server authority requirements below.

---

# 4. Agent Server Requirements

## 4.1 Agent Server role

The Agent Server MUST be the authoritative live state machine for agent execution and MUST own the agentic loop: model-call ordering, hosted-tool coordination, and continuation. Direct model-only chat under section 3.3 is not an agent runtime and does not transfer or duplicate this authority.

The Agent Server MUST own live state associated with its agent execution:

- sessions
- agents
- runs
- conversation state
- agent mailboxes
- model-call lifecycle
- ToolRun lifecycle
- event ordering
- message ordering
- cancellation
- active configuration
- checkpoint generation
- Client subscriptions

The Agent Server MUST support multiple concurrently connected Clients.

---

## 4.2 Identity independence

The following identities MUST remain logically independent:

    ServerId != PID
    SessionId != connection
    SessionId != process
    RunId != connection
    ToolRunId != child process
    ExecutorId != process

A transport connection closing MUST NOT inherently:

- destroy a session
- terminate the Agent Server
- cancel an active run

unless an explicit policy requires it.

---

## 4.3 Session ordering

The Agent Server MUST establish the canonical semantic ordering of state transitions.

Transport arrival order across different connections MUST NOT be treated as a global semantic order.

State mutation within a session SHOULD be serialized through one logical state machine or mailbox.

Different sessions MAY execute concurrently.

The Agent Server SHOULD assign monotonic per-session sequence numbers to committed observable state transitions.

These sequence numbers SHOULD support:

- event replay
- reconnection
- deduplication
- durable persistence
- resuming subscriptions

---

# 5. Daemon Requirements

## 5.1 Daemon model

The Daemon MUST be conceptually implemented as:

> a long-lived Client plus daemon-specific continuity services.

The Daemon's normal interaction with an Agent Server SHOULD use the same Agent Client SDK and Agent Server protocol used by other Clients.

The Agent Server SHOULD NOT require special-case behavioral branches based solely on a peer being a Daemon.

Authority SHOULD instead be represented through explicit capabilities or permissions.

---

## 5.2 Daemon responsibilities

Daemon-specific services MAY include:

- Agent Server process spawning
- Agent Server supervision
- Agent Server adoption
- Agent Server restart
- checkpoint requests
- Agent Server restoration requests
- background scheduling
- background task management
- completion hooks
- multi-agent orchestration
- persistent tool execution

The Daemon SHOULD manage concerns that must survive time or process boundaries.

The Daemon MUST NOT become the authoritative live agent state machine.

Daemon supervision of an Agent Server MUST NOT transfer that Agent Server's Model Provider or Session Store lifecycle responsibilities to the Daemon. Clients, including the Daemon, MUST NOT spawn or directly supervise Providers for operations routed through an Agent Server. Explicit direct CLI hosting under section 3.3 is the sole Client-side Provider-host exception. Clients and Daemons MUST NOT spawn or directly supervise Session Stores or communicate directly with them; all such access MUST remain Agent Server-mediated.

---

# 6. Agent Server Lifecycle

The system MUST support all of the following:

### 6.1 Client-spawned standalone Agent Server

A user Client MUST be able to spawn and use an Agent Server without any Daemon.

    Client
       ↓ spawn
    Agent Server

---

### 6.2 Agent Server adoption

An already-running unmanaged Agent Server MUST be capable of later becoming managed by a Daemon.

Existing Client-to-Agent Server connections SHOULD remain valid during and after adoption.

Adoption MUST NOT require routing ordinary Client traffic through the Daemon.

---

### 6.3 Daemon-spawned Agent Server

The Daemon MUST be capable of spawning an Agent Server itself.

---

## 6.4 Supervision model

The architecture MUST distinguish:

- who spawned the Agent Server
- who currently supervises the Agent Server
- who is connected to the Agent Server

An Agent Server MAY transition conceptually from:

    UNMANAGED
        ↓
    MANAGED

Adoption refers to logical supervision and MUST NOT depend on OS-level process reparenting.

A managed Agent Server's logical identity MUST survive Agent Server process restart.

---

## 6.5 Model Provider and Session Store lifecycle

For operations routed through an Agent Server, Model Provider processes MUST be spawned and supervised by that Agent Server. Originating such an operation MUST NOT give a Client or Daemon responsibility for the Provider process. In explicit direct CLI mode, the CLI instead MUST spawn and supervise its own Provider processes; they MUST remain independent of Agent Server-owned operations and runs. Each host MUST own monitoring, termination, and any restart policy for its own Provider children.

Session Store processes MUST be spawned by the Agent Server. The Agent Server alone MUST own their monitoring, termination, and any restart policy. Direct Provider hosting MUST NOT authorize Session Store access or lifecycle ownership outside the Agent Server.

Standalone Agent Server operation MUST NOT require a Daemon or a pre-existing Model Provider or Session Store service. The Agent Server MUST spawn the configured implementations rather than attach to independently running implementations. Reusing a process that the Agent Server already spawned is distinct from depending on a pre-existing service. Direct CLI operation likewise MUST NOT require an Agent Server, Daemon, or pre-existing Provider service.

Standalone operation means independence from other Moly services, not necessarily offline operation. Hosted Providers MAY access configured model APIs; Agent Server-owned Session Stores MAY access configured storage services such as DynamoDB.

Process lifecycle, authoritative live conversation state, and coordination of access to stored history MUST remain separate responsibilities. Component process failure or restart MUST NOT be treated as proof that an interrupted operation had no effect.

---

# 7. Local Transport

## 7.1 Canonical local IPC

Local IPC SHOULD be the canonical Agent Server transport.

On Unix-like systems, the default implementation SHOULD use:

- Unix domain sockets
- stream semantics

On Windows, the default implementation SHOULD use:

- Windows named pipes
- byte-stream semantics

Transport-specific types MUST NOT leak into the application protocol.

The transport abstraction SHOULD expose connected byte streams compatible with asynchronous read/write semantics.

---

## 7.2 Agent Server endpoint

An Agent Server SHOULD normally expose one logical local endpoint.

One Agent Server endpoint MUST support multiple simultaneous Client connections.

The Agent Server identity MUST NOT be derived from the endpoint path or pipe name.

---

# 8. Daemon Discovery

Clients MUST NOT discover the Daemon by enumerating operating-system processes.

The Daemon SHOULD expose a deterministic, well-known local endpoint.

The endpoint SHOULD be scoped by relevant local identity such as:

- OS user
- installation
- release channel
- optional profile

Discovery SHOULD consist of:

    derive well-known endpoint
        ↓
    connect
        ↓
    protocol handshake
        ↓
    verify daemon identity and compatibility

PID files MAY exist for diagnostics but MUST NOT be authoritative evidence that a Daemon is alive.

---

# 9. Protocol Requirements

Component interfaces MUST be specified by protocol schemas and behavioral semantics rather than language-specific implementation APIs. Protocol schemas and behavioral specifications MUST be available in language-neutral form so independent implementations do not need to link Moly's Rust crates.

## 9.1 Protocol independence

Protocol semantics MUST be independent of:

- UDS
- Windows named pipes
- TCP
- WebSocket
- STDIO
- JSON
- Protobuf

Transport and codec are replaceable implementation layers.

---

## 9.2 Initial encoding

Component protocols MUST support JSON for cross-language communication. The initial framing SHOULD use JSONL:

- UTF-8
- one JSON value per LF-delimited frame

The following MUST NOT be assumed:

    one write == one message
    one read == one message

For JSONL, the framing contract MUST be:

    one LF-terminated frame == one protocol message

Receivers MUST buffer partial reads and split complete frames on LF.

A maximum frame size MUST be enforced.

EOF with an incomplete frame SHOULD be treated as a protocol error.

---

## 9.3 Full-duplex behavior

The Client–Agent Server protocol MUST support concurrent bidirectional operations.

It MUST allow:

    Client → Agent Server requests
    Agent Server → Client responses
    Agent Server → Client events
    Agent Server → Client requests
    Client → Agent Server responses

One outstanding request MUST NOT block unrelated requests on the same connection.

Messages MUST carry correlation identifiers.

---

## 9.4 Protocol evolution

Unknown fields SHOULD generally be tolerated.

Unknown methods MUST result in a structured protocol error.

Each Component protocol MUST define its version and compatibility rules and SHOULD support explicit version negotiation.

The canonical contract is the protocol schema and semantics, not JSON encoding itself.

A Protobuf or other binary codec MAY be added without changing semantic behavior.

---

# 10. Configuration

## 10.1 Configuration ownership

The Agent Server MUST NOT discover or read user configuration files on startup.

A directly started Agent Server MUST begin without reading:

- user config directories
- project config files
- environment-based configuration
- profiles
- configuration inheritance

Loading, editing, merging, and resolving configuration MUST belong to Clients, including the Daemon. This includes selecting and configuring Model Provider and Session Store implementations.

---

## 10.2 Configuration flow

The expected flow is:

    configuration sources
        ↓
    Client / Daemon
        ↓
    load
    merge
    resolve
        ↓
    ResolvedConfig
        ↓
    Agent Server
        ↓
    validate
    apply

The Agent Server MUST receive resolved configuration as data.

The Agent Server SHOULD perform authoritative semantic validation before applying configuration.

Resolved configuration MUST identify the selected Model Provider and Session Store implementations and the information needed for the Agent Server to launch them. Session Store configuration MUST identify its storage namespace/options and any credential references. The Agent Server MUST use the supplied configuration or report a structured error; it MUST NOT discover configuration sources or silently substitute a different implementation.

---

## 10.3 Configuration concurrency

The active Agent Server configuration SHOULD have a revision identifier.

Configuration updates SHOULD use optimistic revision checking so multiple Clients cannot silently overwrite each other.

---

# 11. Model Invocation

Every LLM invocation for an Agent Server-owned agent run MUST be Agent Server-owned and issued through MPP. Explicit direct CLI model-only chat MAY issue MPP invocations without an Agent Server under section 3.3; this exception MUST NOT transfer authority over any Agent Server session or run.

This requirement concerns authority, not necessarily OS process placement.

For Agent Server-run invocations, the actual request MAY execute in:

- the Agent Server process
- a provider child process
- a model worker
- another execution environment controlled by the Agent Server

However:

- Clients MUST NOT independently own agentic loops; direct CLI multi-turn model-only chat is the limited exception to Agent Server-hosted model invocation, not an agentic loop
- the Daemon MUST NOT independently own agentic loops

For its agent runs, the Agent Server MUST retain authority over:

- model-call ordering
- context construction
- cancellation
- usage accounting
- continuation
- retries
- relationship to Run state

---

# 12. Model Providers

## 12.1 Provider scope

A **Model Provider**, shortened to **Provider**, is a Component that adapts Moly's model semantics to an external AI service. Its interface to a host MUST be the **Moly Provider Protocol (MPP)**, not a requirement to link a particular implementation into that host. The host MAY be the Agent Server or the explicit direct CLI described in section 3.3; this does not change the Provider Component role.

Providers MUST NOT be assumed to be simple `generate()` wrappers.

A Provider MAY need to handle:

- authentication
- credential interpretation
- credential refresh
- user interaction
- request signing
- provider-specific headers
- sticky session affinity
- routing metadata
- model discovery
- model-specific options
- provider-native tools/features
- streaming normalization
- continuation semantics
- usage accounting

---

## 12.2 Provider isolation

Provider implementations MUST NOT directly depend on application topology such as:

- CLI
- browser
- Daemon
- Client type
- hosted Tool Executor

Providers SHOULD depend only on MPP and generic host services.

Generic host capabilities MAY include:

- `HttpTransport`
- `SecretStore`
- `Interaction`
- time/clock services

A Provider MUST NOT need to know which concrete implementation supplies these services. Any host-service interaction crossing a Component boundary MUST use protocol messages rather than language-specific objects or callbacks.

---

## 12.3 Provider authentication

Provider-specific authentication semantics SHOULD remain inside the Provider implementation.

The architecture MUST allow arbitrarily provider-specific behavior without leaking that behavior into Agent Server Core.

User-facing interactions SHOULD be exposed through generic interaction requests such as:

- open URL
- show code
- request secret
- request text input
- wait for completion

---

# 13. Inference Context and Session Affinity

The MPP host SHOULD provide generic inference context to Providers, including identities such as:

- SessionId
- RunId
- ModelCallId
- call kind

For Agent Server runs these identities MUST be Agent Server-owned; direct CLI identities MUST be local correlations without Agent Server session/run authority. Provider-specific request metadata MUST be derived by the Provider, not Agent Server Core or the direct CLI.

Examples include:

- routing affinity IDs
- provider conversation IDs
- proprietary headers
- sticky routing metadata

The following concepts MUST remain distinct:

    SessionId
        Moly conversation identity

    AffinityKey
        routing locality identity

    CacheKey
        reusable prompt-prefix identity

Providers MAY map these concepts differently.

---

# 14. Provider-native Features

Provider-native capabilities MUST NOT automatically be treated as ordinary hosted tools.

The system MUST distinguish conceptually between:

### Hosted Tool

Executed by a Moly Executor.

    Agent Server
        ↓
    Executor

### Provider Feature

Executed within or by an upstream model provider.

    Agent Server
        ↓
    Provider

Examples of Provider Features may include:

- provider web search
- provider code interpreter
- provider file search
- provider computer-use facilities

The Provider SHOULD expose availability/capability metadata.

For its agent runs, the Agent Server decides which features are enabled for a ModelCall. Direct model-only chat grants no hosted-tool execution authority.

---

## 14.1 Model continuation

Provider behavior MAY require multiple inference steps.

Do not encode this as a provider-specific boolean.

Model execution SHOULD support generic outcomes such as:

    Completed
    AwaitHostTools
    Continue
    Failed

Provider continuation state SHOULD remain opaque where possible.

Observable provider events and continuation decisions MUST be separate concepts.

---

# 15. Tools and Executors

## 15.1 Tool abstraction

A Tool defines what capability is available.

An Executor defines where and how that capability produces side effects.

These concepts MUST remain distinct.

Potential Executors include:

- Local Executor
- Client Executor
- Daemon Executor
- Browser Executor
- remote sandbox
- WASM Executor

---

## 15.2 ToolRun ownership

The Agent Server MUST be authoritative for ToolRun lifecycle.

The Executor MUST own the physical side effect or process.

Conceptually:

> Agent Server owns execution logically.
> Executor owns execution physically.

ToolRun state MAY include:

- ToolRunId
- tool identity
- arguments
- ExecutorId
- execution generation/lease
- state
- result

---

## 15.3 Standalone execution

Agent Server Core SHOULD NOT need hard-coded side-effect execution.

The standalone Agent Server binary MAY compose:

    Agent Server Core
    +
    Local Executor

This MUST permit a directly invoked standalone Agent Server to execute tools without requiring a Daemon.

The Local Executor SHOULD use the same conceptual Executor boundary as Client and Daemon executors.

---

## 15.4 Client-provided tools

The Agent Client SDK SHOULD support dynamically providing tools to an Agent Server.

A Client tool definition SHOULD include:

- name
- description
- input schema
- execution callback/capability
- relevant metadata

When invoked:

    model
        ↓
    Agent Server creates ToolRun
        ↓
    Agent Server sends reverse execution request
        ↓
    Client executes tool
        ↓
    Client returns result/events
        ↓
    Agent Server commits ToolRun result

This mechanism should support browser-defined tools.

---

# 16. Daemon-provided Tools

The Daemon SHOULD be capable of providing tools through the same Agent Client SDK tool-hosting mechanism.

Examples MAY include:

- background scheduling
- background execution
- agent message sending
- agent orchestration operations

Daemon-specific implementation details MUST remain outside Agent Server Core.

A daemon-provided tool MAY internally issue ordinary Agent Client SDK commands back to the Agent Server.

---

# 17. Background Tasks

Temporal scheduling SHOULD belong to the Daemon rather than Agent Server Core.

Examples:

- run after delay
- run at specified time
- retry later
- send message later

At execution time, the Daemon SHOULD issue ordinary Client protocol operations such as:

- `run.start`
- `message.send`

The resulting agent run remains Agent Server-owned.

---

# 18. Multi-Agent Behavior

The Agent Server MUST own:

- agent identities
- live agent state
- mailboxes
- message delivery
- message ordering

The Daemon MAY own higher-level orchestration policies.

For example:

    Agent A completes
        ↓ event
    Daemon observes
        ↓
    Daemon sends message command
        ↓
    Agent Server
        ↓
    Agent B mailbox

The Agent Server SHOULD NOT hard-code arbitrary orchestration policy.

---

# 19. Persistence

## 19.1 Ownership and scope

- The Agent Server MUST own live conversation state, history identities and ordering, execution-branch selection, context construction, and restoration semantics.
- The Agent Server MUST invoke discovery, saving, loading, and session-ownership operations through the Session Store protocol using the implementation selected by resolved configuration.
- The Session Store MUST own persistence and coordinate access to stored sessions without becoming the authoritative live agent state machine. Loading returns history data for the Agent Server to validate and activate.
- Clients, including the Daemon, MAY originate these operations through the public Agent Server protocol. They MUST NOT communicate directly with the Session Store or bypass the Agent Server to mutate its authoritative history. Clients MAY handle exported history files.
- Saving and restoring history MUST NOT require a Daemon. A Client disconnecting MUST NOT itself release the Agent Server's session ownership or stop Agent Server-owned persistence.
- Conversation history, model context, and complete runtime checkpoints MUST remain distinct concepts.

---

## 19.2 Tree-structured history

Session history MUST use an append-only tree of typed entries, following the approach described in [Pi's session format](https://github.com/earendil-works/pi/blob/v0.99.2/packages/coding-agent/docs/session-format.md). Byte-for-byte compatibility with Pi is not required.

- Each entry MUST have a stable identity, an entry type, a timestamp, and a parent entry reference or an explicit root marker.
- Entry identities and parent relationships MUST survive saving, restoration, and storage migration. They MUST NOT depend on filesystem paths, database locations, processes, or connections.
- Parent relationships MUST be acyclic and resolvable within the session. Entry identity, ancestry, Agent Server-assigned commit order, and timestamps MUST remain distinct; storage order or timestamps MUST NOT replace canonical semantic ordering.
- Continuing from an earlier entry MUST create a new branch without deleting or modifying the abandoned path. Normal appends, branching, compaction, and context edits MUST preserve earlier logical entries.
- Forking a selected path into another session MUST create a new session identity and preserve provenance to the source session and branch point.
- The selected execution branch/head MUST be recorded explicitly and restored independently of physical record position. Restoration MUST NOT infer the selected branch solely from the last stored record.
- A Client's browsing position MUST remain distinct from the Agent Server's execution branch. Branch changes affecting execution MUST be serialized with run state changes and MUST NOT silently redirect an active run.

---

## 19.3 Stored history and model context

History MUST preserve supported conversation data, including:

- user and assistant messages, supported content blocks, and tool calls/results with their correlations and failure states
- system prompt and tool-definition changes needed to reconstruct context
- model attribution, usage, and response termination information when available
- branch, compaction, context-edit, and session-metadata records

Additional requirements:

- Attachments MUST remain available after restoration through stored content or durable references. Temporary local paths alone MUST NOT be treated as portable attachment storage.
- Provider-specific metadata required to replay supported messages MUST be preserved as opaque data. Agent Server Core and Session Store implementations MUST NOT need to interpret provider-specific encoding.
- Model context MUST be derived from the selected branch and its context-affecting records, not from every entry in the stored session.
- Compaction MUST append a summary with an explicit retained-history boundary and the prompt/tool state needed to rebuild context. It MUST NOT delete the summarized raw history.
- Context edits MUST be recorded as branch-relative append-only operations. They MAY omit or replace content in subsequent model context but MUST preserve the original transcript and its accounting metadata.
- Names, labels, usage records, and extension state MUST remain distinguishable from messages intended for model context. Merely persisting metadata MUST NOT make it model-visible.

---

## 19.4 History format and serialization

The initial [conversation history v1 contract](conversation-history.md) defines a
limited independently versioned snapshot representation. Its pure validator is not
Session Store ownership, durable saving, runtime restoration, or full compliance
with the persistence requirements below.

- The canonical history schema MUST be versioned independently of Component protocols, Rust APIs, and implementation-specific storage layouts.
- Session metadata MUST include the format version, logical session identity, and creation time. It SHOULD include a display name, workspace association, and fork provenance where applicable.
- Filesystem storage SHOULD use UTF-8 JSONL with a session header followed by typed records. JSONL MUST also be supported as a portable history import/export representation.
- The Session Store protocol MUST describe history operations and records, not require a filesystem or a byte-stream append API. Database-based implementations MAY store structured records while preserving the same semantics.
- Portable exports MUST declare whether they contain the complete tree or a selected branch. They MUST preserve the selected head and all records and attachment references needed to restore that scope.
- Persisted history MUST NOT be defined as a verbatim capture of transport envelopes, streaming deltas, or a bounded event-replay buffer.

---

## 19.5 Replaceable Session Store implementations

A **Session Store** is a Component responsible for saved sessions, their usage/ownership status, and stored checkpoints, distinct from Model Providers and Tool Executors. Implementations MAY use a filesystem, in-memory storage, or an external storage service such as DynamoDB.

- The Agent Server MUST support interchangeable implementations through the same Session Store protocol. Client-side configuration SHOULD default to a filesystem implementation and MAY select DynamoDB or other supported implementations without changing history semantics or Client-facing operations.
- An in-memory implementation SHOULD support hermetic tests and explicitly ephemeral sessions. It MUST NOT claim durable persistence or ownership coordination beyond its instance.
- The Session Store protocol MUST cover session creation and discovery, metadata, paginated history reads, ordered appends, selected-head updates, persistence acknowledgments, and usage/ownership operations. Explicit history deletion SHOULD be supported subject to retention policy.
- The session store MUST report, for each session, whether it is available for activation, in use, or of unknown availability. In use means a valid ownership claim exists under the store's ownership rules, not merely that history exists or a Client is connected. Status MUST NOT imply proof that the owning process is alive.
- The session store MUST support atomic ownership acquisition and release. A preceding availability query MUST NOT authorize activation by itself. Competing Agent Servers MUST NOT both acquire valid ownership of the same session within the store's coordination scope.
- Each implementation MUST define that coordination scope, ownership-loss detection, and stale-owner recovery behavior. Storage-specific locks or leases MAY implement these rules without a separate application-wide attachment registry.
- The session store MUST reject mutations from invalidated owners. An Agent Server that loses ownership or cannot establish its validity MUST stop initiating new session mutations and execution. After reacquiring ownership, it MUST reconcile live state with committed history before resuming work. These rules MUST NOT be interpreted as undoing physical side effects already initiated.
- Storage-service SDK types, lock handles, and database-specific queries MUST remain behind the Session Store protocol rather than leak into canonical history or the public Agent Server protocol. Usage status MUST be available through Agent Server queries without exposing storage mechanisms.
- Storage access credentials MUST be supplied separately from saved history and portable exports. Session-store initialization and remote access MUST preserve the lazy CLI startup requirements in section 3.2.
- Third-party implementations MUST be substitutable through the Session Store protocol as specified in section 25.

---

## 19.6 Saving, concurrency, and durability

- Persistent sessions MUST save incrementally. Saving an accepted user message MUST NOT depend on receiving an assistant reply. Completed model/tool messages and other committed history changes MUST be saved without requiring successful completion of the entire run.
- The system MUST distinguish the latest live commit from the latest acknowledged durable commit. Each persistent implementation MUST define the failure model covered by its durability acknowledgment; a successful live-state mutation alone MUST NOT imply durability.
- The session store MUST enforce expected-revision checks or equivalent conflict detection for appends and selected-head updates, in addition to validating ownership. Concurrent or stale writers MUST NOT silently overwrite history or lose updates.
- Persistence operations MUST support idempotent retries using stable operation identities. Retrying after an uncertain outcome MUST NOT duplicate entries; reusing an identity with different content MUST fail explicitly. This does not imply exactly-once tool execution.
- Readers MUST observe a consistent committed history revision, including its selected head, rather than partial logical writes. A durability acknowledgment MUST NOT advance past uncommitted or missing required records.
- Storage failures MUST distinguish absence, access denial, unavailability, conflict, corruption, and uncertain write outcomes. Failure MUST NOT be presented as an empty conversation or a successful save.
- Ephemeral operation MUST be an explicit policy. Storage failure MUST NOT silently downgrade a persistent session to ephemeral operation.

Sharing stored data MUST NOT be equated with sharing a running Session Store process. If independent Agent Servers access the same stored sessions, their Session Store implementations MUST coordinate ownership within the declared scope. This coordination MUST NOT require a Daemon or transfer live conversation authority out of the Agent Server. The coordination mechanism MUST remain behind the Session Store protocol; no particular shared-process or storage-primitive design is required by this contract.

---

## 19.7 Discovery and restoration

- Clients MUST request saved-session discovery, usage status, and restoration through the Agent Server. The Agent Server MUST obtain this information from the configured Session Store, not infer usage from Client connection counts. The Agent Server MUST return the requested session information to the requesting Client through the public Agent Server protocol.
- Users MUST be able to list, open, and resume saved conversations by logical identity. Discovery SHOULD support names, workspace association, and recent activity without requiring users to know storage-specific paths or keys.
- Before activating restored history for live use, the Agent Server MUST acquire ownership through the configured session store and load a consistent committed revision under that ownership. A session owned by another Agent Server MUST yield an explicit in-use result rather than an independent writable copy. Unknown availability MUST NOT be treated as available.
- If the session is already active in the requesting Agent Server, ordinary open/resume requests MUST reuse its live state rather than replace it with older persisted data. Explicit replacement MUST be coordinated with active runs and state revisions, and affected Clients MUST be notified.
- Restoration MUST preserve session identity, raw history, branch relationships, the selected head, and recorded context-affecting state. Creating a new identity from existing history MUST be an explicit fork or copy operation.
- Listing, reading, and restoring history MUST NOT require a working model provider or model credentials. Continuing inference MAY require provider configuration, credentials, and available tools.
- Unavailable models, tools, attachments, or required replay metadata MUST be reported explicitly. The system MUST NOT silently rewrite history or substitute execution capabilities to make a restored conversation appear complete.
- Restoring a conversation MUST NOT itself invoke a model, execute tools, revive connections or executor leases, or resume physical side effects.
- Interrupted runs and tool calls with unestablished outcomes MUST remain distinguishable from completed work. Any execution recovery MUST follow an explicit Agent Server-owned policy rather than treating a missing result as permission to repeat a side effect.

For example, session discovery follows:

    Client / Daemon → list sessions → Agent Server → list sessions → Session Store
    Client / Daemon ← response      ← Agent Server ← response      ← Session Store

The Client or Daemon originates this operation. If a Session Store process needs to be spawned to serve it, the Agent Server spawns that process. Neither request forwarding nor process spawning implies a new handshake for every operation.

---

## 19.8 Compatibility, integrity, and migration

- Loaders MUST validate required fields, entry identities, parent references, selected heads, and supported format versions before activating restored history. Unsupported semantic records MUST NOT be silently discarded.
- Recovery from an incomplete final write MAY retain a verified committed prefix, but MUST report the recovery boundary and warn that later writes may have been lost. Malformed middle records, broken ancestry, cycles, and conflicting identities MUST NOT be silently skipped.
- Format migration MUST preserve logical history semantics and MUST NOT destroy the only valid copy if migration is interrupted or fails.
- Selecting another Session Store implementation MUST remain distinct from migrating existing history. A configuration change MUST NOT silently move, replace, fork, or reinterpret existing sessions. Changes affecting an active session's storage or ownership MUST perform an explicit coordinated transition or be rejected; active writes MUST NOT be silently redirected.
- Storage migration MUST copy a consistent source revision and preserve identities, branches, the selected head, metadata, and attachment availability. Migration MUST respect ownership in both source and destination, and the destination MUST be validated before the source may be retired.
- Explicit deletion and retention policies MUST remain separate from model-context compaction. Removing entries from model context MUST NOT imply deleting their persisted history.

---

## 19.9 Runtime checkpoints

- The Agent Server MUST define complete runtime checkpoint and restoration semantics separately from conversation-history reconstruction.
- The Agent Server SHOULD store and load its checkpoints through the Session Store protocol without requiring implementations to understand internal live-state semantics. A Daemon MAY request checkpointing or restoration as an ordinary Client.
- Checkpoints MUST identify the state revision and history boundary they represent. Durable checkpoint acknowledgments MAY identify the corresponding sequence numbers.
- A Session Store implementation MAY also store checkpoints, but saving a conversation MUST NOT imply that agent mailboxes, active runs, processes, or executor state can be resumed.

---

## 19.10 Conformance

Every persistent Session Store implementation MUST pass a shared, implementation-independent conformance suite covering:

- save/load round trips, discovery, pagination, and metadata
- branch selection, forking, compaction, context edits, and attachment preservation
- implementation selection from supplied configuration without implicit discovery or fallback
- Agent Server-mediated discovery and responses, with Agent Server-owned process spawning
- available/in-use/unknown status, competing ownership claims, release, and stale-owner fencing
- revision conflicts, duplicate retries, uncertain outcomes, and partial-write recovery
- corruption and unsupported-version handling
- portable export/import and storage migration
- restoration without model calls or tool side effects

History fixtures and session-store conformance tests MUST survive implementation rewrites.

---

# 20. Remote Access

The Agent Server SHOULD fundamentally expose a local protocol.

Remote exposure SHOULD normally be implemented outside Agent Server Core.

Preferred structure:

    Remote Client
        ↓
    Gateway / Relay
        ↓
    local IPC
        ↓
    Agent Server

The remote Gateway MAY own:

- TLS
- remote authentication
- authorization
- rate limiting
- network policy
- CORS/CSRF
- audit
- WebSocket/HTTP adaptation

Agent Server Core SHOULD receive normalized identity/capability context rather than provider-specific remote authentication logic.

Direct TCP support MAY exist as an optional transport implementation.

---

# 21. WASM

WASM MUST NOT be required for the initial architecture.

Communication protocol and execution environment MUST remain separate concerns.

WASM MAY be used for:

- tool sandboxing
- portable Executors
- untrusted plugins
- browser/native shared execution
- capability-restricted modules

---

# 22. Observability

The Rust implementation MUST use `tracing`.

Rust library/internal modules MUST emit tracing events/spans but MUST NOT install global subscribers.

Each Rust executable MUST configure its own subscriber. Implementations in other languages MAY use their native logging facilities without depending on Rust.

Client, Agent Server, Daemon, Model Provider, and Session Store logging SHOULD remain independently configurable.

Shared correlation identifiers SHOULD include relevant IDs such as:

- ServerId
- ConnectionId
- SessionId
- RunId
- RequestId
- ModelCallId
- ToolRunId

Protocol events and tracing logs MUST remain separate concepts.

If STDIO protocol mode exists:

    stdout = protocol/data
    stderr = diagnostics

Secrets MUST NOT be logged by default.

---

# 23. Public Compatibility Surfaces

The project SHOULD distinguish the following compatibility surfaces:

## Product

User-facing binaries, CLI behavior, and configuration UX.

## SDK

Developer-facing programmatic APIs.

## Protocol

Process/language boundaries and wire semantics, including the Client–Agent Server protocol, MPP, and the Session Store protocol. These are extension contracts for independent implementations, not Rust API or native ABI compatibility promises.

## Persistence

Versioned history and checkpoint formats, restoration behavior, and portable import/export semantics, independent of wire and Rust API compatibility.

## Internal

Implementation details with no compatibility guarantee.

Crates SHOULD be classified by compatibility surface rather than merely implementation location.

---

# 24. SDK Responsibilities

## 24.1 Agent Client SDK layers

The Agent Client SDK accesses the Agent Server protocol. It MUST NOT spawn processes or own an agentic loop; application process-startup policy remains outside the SDK. Client APIs SHOULD be layered by responsibility, with the capabilities described below.

### Protocol/schema

Provides:

- protocol types
- schemas
- version definitions
- error definitions

### Wire Client

Provides:

- connection management
- request/response correlation
- reverse requests
- event streaming
- multiplexing

### Headless Client

Provides higher-level concepts:

- sessions
- runs
- messages
- subscriptions
- configuration
- tool hosting

Browser, CLI, IDE integrations, and Daemon SHOULD be built on the headless Client where practical.

These layers do not all need separate crates until real dependency boundaries justify them.

## 24.2 MPP host SDK

A separate common MPP host SDK SHOULD support the Agent Server and explicit direct CLI without depending on either executable or the Agent Client SDK. It MUST communicate with independent Provider peers through MPP rather than import Provider executable implementations.

The MPP SDK MAY launch and supervise an explicitly resolved Provider command. Its transport MUST remain private. Configuration discovery, credential persistence, UI, conversation history, tool effects, and agent/run policy MUST remain host responsibilities. It MUST NOT own an agentic loop, become an agent runtime, or act as a Session Store client.

The host MUST own credential slots and serialize every operation sharing a renewable credential for the full operation. Scoped replacements MUST remain committed even if the surrounding operation later fails or is cancelled. The SDK MUST NOT own a secret store.

---

# 25. Model Provider and Session Store Extension Surfaces

Model Providers and Session Stores MUST support third-party implementations through their respective versioned, schema-defined protocols.

An independent developer MUST be able to implement either Component in another programming language and configure the Agent Server to use it without rebuilding the Agent Server. An independent Provider MUST also be usable by an explicitly configured direct CLI through MPP without rebuilding the CLI; this MUST NOT extend direct access to Session Stores. Cross-language communication MUST use protocol messages, with JSON support as specified in section 9, without requiring Rust linkage, WASM, a C ABI, or a dynamic-library ABI.

Bundled implementations MUST preserve the same replaceable protocol contracts. Compile-time registration or a language-specific plugin API alone MUST NOT satisfy this extension requirement.

MPP APIs SHOULD remain experimental until multiple substantially different implementations have exercised them. Experimental protocols MUST still document their schemas, semantics, versions, and compatibility rules; cross-language extensibility does not imply a permanent stability guarantee.

---

# 26. Crate Boundaries

Crate count SHOULD be minimized until real dependency or compatibility boundaries emerge.

Crate responsibilities SHOULD be separated as follows:

- `moly-protocol`: shared wire/schema contract, with no transport or executable implementation dependencies.
- `moly-client`: Agent Client SDK, with private Agent Server-protocol transport and no process spawning.
- `moly-provider-client`: common MPP host SDK, with private Provider transport and explicit Provider process supervision; no agentic loop or secret store.
- `moly-server`: Agent Server executable, with Agent Server Core, credential authority, Executor integration, and Agent Server-side transport kept internal; uses protocol and the MPP host SDK, never the Agent Client SDK.
- `moly`: user-facing CLI executable; uses the Agent Client SDK for Agent Server access and the MPP host SDK only for explicit direct mode.
- Bundled Providers: binary-only, independent MPP implementations; upstream HTTP and OAuth remain inside those executables.

Both SDKs MUST depend only on `moly-protocol` among project crates. Executables MUST NOT depend on another executable's implementation, including for tests or by source inclusion. The CLI MUST communicate with the Agent Server through the public protocol and MUST NOT depend directly on `moly-server` implementation APIs.

A dedicated internal session-store crate MUST encapsulate the Agent Server's Session Store protocol client rather than require storage implementations to be linked into the Agent Server. It MUST NOT depend on executable internals or the Client SDK. Clients MUST access these capabilities through the Agent Server protocol rather than bypassing the Agent Server through that crate.

Rust crate organization MUST NOT impose Rust dependencies on independently implemented Model Providers or Session Stores. Their extension boundary MUST remain the language-neutral protocol; Component boundaries do not prescribe one crate or executable per implementation.

The Agent Client SDK and MPP host SDK MUST remain distinct responsibilities: Agent Server session/run access is not direct Provider access. Their separation MUST NOT require independently implemented Providers or Session Stores to use Rust.

---

# 27. Distribution

Architectural binary boundaries do not need to equal distribution boundaries.

Multiple binaries MAY be distributed together as one product release.

Product binaries SHOULD initially use lockstep release versions.

For example:

    moly
    moly-server
    moly-daemon

MAY be shipped together even though they are separate executables.

---

# 28. Architecture Iteration

The project explicitly permits architectural rewrites.

Repository structure MAY include:

    crates/
    docs/
    conformance/
    archive/

When an architecture reaches a meaningful limit:

1. document the observed failure
2. extract the new invariant
3. tag the revision in Git
4. archive the old architecture generation
5. remove archived code from the active Cargo workspace
6. implement the next architecture generation cleanly

Archived implementations SHOULD be treated as immutable historical references.

Each archived architecture SHOULD document:

- architectural model
- what worked
- observed limit
- newly discovered invariant
- successor architecture

---

# 29. Long-Lived Conformance Assets

The following SHOULD survive rewrites:

- Component protocol schemas, fixtures, and cross-language conformance cases
- JSONL traces
- history-format fixtures and session-store conformance tests
- state transition tests
- compatibility tests
- benchmarks
- architectural invariants

Implementation code itself is not considered sacred.

A rewritten implementation SHOULD be testable against retained conformance artifacts.

---

# 30. Non-Goals for Initial Versions

The initial implementation does NOT need:

- distributed consensus
- Raft
- leader election
- multi-region replication
- cluster-wide scheduler
- exactly-once distributed tool execution
- stable dynamic plugin ABI
- mandatory WASM
- mandatory Protobuf
- mandatory Daemon
- Agent Server-side config file discovery

These MAY be revisited only when concrete requirements justify them.

---

# 31. Initial Vertical Slice

The first implementation SHOULD remain small.

Recommended initial composition:

    CLI Client
        ↓
    local IPC
        ↓
    Agent Server Binary
        ├ Agent Server Core
        └ Local Executor

It SHOULD demonstrate:

- immediate CLI rendering
- lazy Agent Server startup
- local IPC
- JSONL framing
- handshake
- logical IDs
- at least one session
- at least one run
- one Model Provider invoked through its protocol
- one local Tool Executor
- configuration supplied by Client
- structured tracing
- clean Client/Agent Server protocol boundary

Daemon functionality SHOULD be added after this path works and the first architectural pressures are observable.

---

# 32. Core Architectural Invariants

The following summarize the core requirements above:

1. Agent Server owns the agentic loop and is the authoritative live state machine for agent execution; direct model-only chat is not an Agent Server runtime.
2. Agent Server is multi-client.
3. Daemon is a Client plus continuity services.
4. Client, Agent Server, Daemon, Model Provider, and Session Store remain logically distinct protocol participants.
5. Agent Server does not discover configuration files.
6. Client or Daemon sends resolved configuration to Agent Server.
7. Every LLM invocation for an Agent Server run is Agent Server-owned; explicit direct CLI model-only chat uses MPP without taking over Agent Server authority.
8. Provider-specific complexity stays behind Provider boundaries.
9. Provider does not know Client/Daemon topology.
10. Hosted tools and provider-native features are distinct.
11. Agent Server owns ToolRun state.
12. Executor owns physical side effects.
13. Standalone Agent Server may bundle a Local Executor.
14. Clients may provide tools.
15. Daemon may provide tools through the Agent Client SDK.
16. Protocol semantics are independent of transport.
17. JSONL is an initial encoding, not an architectural dependency.
18. Semantic ordering is established by Agent Server state transitions.
19. Logical IDs are independent from OS resources.
20. Internal implementation may be rewritten freely.
21. Public protocol behavior and conformance assets should survive rewrites.
22. Conversation history is an append-only tree; model context is a derived view, not a replacement for history.
23. The Agent Server invokes a replaceable Session Store through its protocol using Client-supplied configuration; Clients and Daemons access it only through the Agent Server.
24. Restoring conversation history does not implicitly resume runs or repeat physical side effects.
25. Session Stores report usage and coordinate exclusive ownership; Client attachment is not ownership.
26. The Agent Server spawns and supervises Providers for operations routed through it; an explicit direct CLI hosts only its own Providers. Session Store processes and access remain Agent Server-only. Neither path requires a Daemon or pre-existing Component services.
27. Operation origination, process spawning, and handshake initiation are separate concepts.
28. MPP and Session Store protocols support cross-language implementations without rebuilding the Agent Server or requiring WASM or a native ABI; MPP also permits explicit direct CLI hosting without changing Provider roles.

These requirements are intentionally subject to revision when implementation experience reveals that an underlying assumption is incorrect.

When that happens, prefer documenting the newly discovered constraint and revising the architecture cleanly rather than accumulating compatibility hacks inside internal implementations.
