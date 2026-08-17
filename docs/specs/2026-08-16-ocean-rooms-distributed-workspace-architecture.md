# Ocean Rooms distributed workspace architecture

**Status:** operator-directed concept architecture; not an implementation manifest  
**Date:** 2026-08-16  
**Scope:** room-authorized agents, computers, folders, and distributed execution  
**Implementation status:** proposed; current persistent Rooms do not provide this distributed resource fabric

## 1. Purpose

Ocean Rooms should be more than a durable group transcript. A room should define a secure collaboration boundary in which people can:

- authorize agents as room members;
- contribute selected folders and compute environments from their Ocean computers;
- let those room-authorized agents work across all contributed computers;
- preserve local custody, permission enforcement, cancellation, and auditability on every machine; and
- coordinate durable work without turning a hosted room service into a remote shell or bulk file relay.

The resulting product is a **room-scoped distributed workspace**. Every participating Ocean installation can contribute bounded resources. Agents see one logical room namespace and can route work to the computer that owns the required files or environment.

This document explains the intended architecture. It does not authorize routes, schemas, migrations, or implementation stages by itself.

## 2. Product statement

> An Ocean Room joins people, agents, computers, folders, and durable work under one authorization boundary.

A room member can dedicate a local folder to the room. The room does not upload or copy that folder by default. Instead, the member's Ocean daemon advertises a safe resource projection and retains the private binding from that projection to the real local path.

A room-authorized agent can then address the resource through a logical URI:

```text
room://launch/alice/source/
room://launch/bob/designs/
room://launch/team-linux/data/
room://launch/build-mac/release/
```

Ocean resolves each URI to the computer that owns it. Small file operations may execute as bounded remote capability calls. Larger tasks should execute near the data on the owning computer and return structured progress, patches, artifacts, and results to the room.

From the agent's perspective, the room is one distributed workspace. From the operator's perspective, every machine remains an independent enforcement authority.

## 3. Example

Consider a `Product Launch` room:

```text
People
  Alice
  Bob

Authorized agents
  Researcher       mention-only
  Builder          task + thread replies
  Release Manager  explicit invocation only

Computers and resources
  Alice's MacBook
    source          ~/Projects/product       read/write
    references      ~/Documents/research     read-only

  Bob's Mac
    designs         ~/Design/Ocean           read/write

  Team Linux
    data            /srv/launch-data          read/write + execute

  Build Mac mini
    release         /opt/build/product        read/write + execute
```

A room task could ask Builder to:

1. read the product source on Alice's MacBook;
2. inspect exported assets on Bob's Mac;
3. run data validation on Team Linux;
4. apply a reviewed patch to Alice's source tree;
5. build and sign on the Build Mac mini; and
6. publish the release manifest and build result into the room.

The coordinating agent does not need every file copied into its own process. Ocean moves the bounded operation or worker to the resource-owning machine whenever that is safer or more efficient.

## 4. Core concepts

### 4.1 Room

A durable collaboration and authorization boundary. A room owns membership, agent bindings, resource projections, durable messages, artifacts, and audit facts.

A room is not itself an execution host. It coordinates independently enforced Ocean nodes.

### 4.2 Ocean node

One enrolled Ocean installation on a computer. A node has a stable device identity, advertises availability, owns local resources, executes locally authorized operations, and publishes sanitized results.

A user may enroll multiple nodes in the same room. A shared workstation may be represented by a service-owned node rather than a person's laptop.

### 4.3 Agent definition

An installed package describing an agent's instructions, model preference, tools, and requested capabilities. Installation makes an agent a **candidate**; it does not authorize the agent in any room.

### 4.4 Room-agent binding

A durable authorization that turns an installed agent candidate into a room member with room-specific identity, context, activation policy, and grants.

An agent name existing globally is insufficient. Every room-triggered execution must resolve an active room-agent binding.

### 4.5 Room resource

A safe, room-visible projection of a resource owned by one Ocean node. The first resource kind is a bounded folder. Later kinds may include a repository checkout, build environment, GPU lane, browser profile, dataset, or device capability.

The projection is federated. The private local path, credentials, device secrets, and enforcement handles are not.

### 4.6 Resource grant

The local authority connecting a room resource to a real folder or compute environment. The owning node enforces the grant for every operation.

### 4.7 Coordinating turn and remote worker

A coordinating turn interprets the operator's request and selects resources. A remote worker is a bounded execution launched on another Ocean node near the relevant resource. Worker graph policy, retries, fan-out, and role definitions remain extension-owned rather than becoming a fleet scheduler in Ocean core.

## 5. System topology

```text
                         durable room control plane
                   membership / grants / events / audit
                                      |
             +------------------------+------------------------+
             |                        |                        |
       Alice's Ocean             Bob's Ocean            Build Mac Ocean
       node:alice-mbp             node:bob-mac            node:build-mini
             |                        |                        |
       source folder              design folder            build folder
             |                        |                        |
             +========= secure direct Ocean transport =======+
                      resource operations and workers
```

The architecture separates two planes.

### 5.1 Control plane

The room control plane carries:

- room and member identity;
- agent and node membership;
- safe resource projections;
- grant status and revocation facts;
- availability and bounded health state;
- task requests and durable lifecycle events;
- room messages, artifacts, and audit records; and
- references to results stored or transferred elsewhere.

For an authorized Rising Tides deployment, Ocean Bedrock may implement the authenticated shared room service. Public Ocean must retain a usable local or public coordinator path and must not require a private repository or private service.

The coordinator never becomes local execution authority.

### 5.2 Data and execution plane

The direct node-to-node plane carries:

- bounded file reads and metadata queries;
- patches and explicitly authorized writes;
- remote execution requests;
- worker lifecycle, cancellation, logs, and results;
- content-addressed artifacts; and
- resumable transfers when a result cannot be represented as a patch or compact artifact.

Tailscale is the preferred first-class secure networking primitive for discovery and encrypted reachability across participating machines. Room membership alone does not prove network identity: Ocean must bind the room member, Ocean node identity, and authenticated tailnet peer before admitting a request.

Large source trees and command output should not traverse the hosted room coordinator when a direct authenticated path is available.

## 6. Identity and authorization

Four identities must remain distinct:

1. **Human principal** — the room member who authorizes resources and agents.
2. **Room agent member** — the room-scoped identity used in messages and tasks.
3. **Ocean node** — the computer enforcing a resource grant.
4. **Execution instance** — one coordinating turn or remote worker with a bounded lifetime.

Authorization is an intersection, never a union:

```text
effective authority =
    room-agent binding
  ∩ resource grant
  ∩ node policy
  ∩ agent package request
  ∩ runtime permission decision
  ∩ execution-instance budget
```

No layer may widen another layer.

An agent authorized in a room is not automatically authorized for every room resource. A folder contributed to a room is not automatically writable. A node accepting reads is not automatically an execution target.

## 7. Proposed records

The following records are conceptual. Exact wire and storage schemas require a separately reviewed implementation manifest.

### 7.1 Room-agent binding

```text
RoomAgentBinding
  room_id
  member_id
  agent_package_id
  agent_definition_revision
  display_name
  owner_member_id
  authorized_by
  authorized_at
  activation_policy
  context_policy
  requested_capabilities
  room_capability_grants
  memory_scope
  status
```

Important properties:

- the package revision or content digest is pinned at authorization time;
- changing requested capabilities requires re-authorization;
- removal or revocation prevents new execution immediately;
- stable room-agent identity is separate from any individual worker id; and
- credentials and provider configuration remain local to the executing Ocean node.

### 7.2 Ocean node enrollment

```text
RoomNodeEnrollment
  room_id
  node_id
  owner_member_id
  display_name
  device_public_identity
  authenticated_network_identity
  supported_capability_classes
  availability
  enrolled_at
  last_confirmed_at
  status
```

The room projection must not expose private hostnames, local usernames, absolute paths, tailnet addresses, or credentials unless an explicit operator-facing diagnostic contract permits them.

### 7.3 Local resource grant

```text
LocalRoomResourceGrant
  room_id
  resource_id
  owner_node_id
  local_root
  resource_kind
  access_mode
  authorized_agent_member_ids
  tool_policy
  command_policy
  approval_policy
  transfer_policy
  expires_at
  generation
  status
```

This record remains on the owner node. `local_root` is never federated.

### 7.4 Safe resource projection

```text
RoomResourceProjection
  room_id
  resource_id
  owner_member_id
  owner_node_id
  display_name
  resource_kind
  access_mode
  capability_classes
  availability
  grant_generation
  status
```

A generation prevents stale requests from surviving revocation and re-grant. The resource owner increments it whenever authority changes.

### 7.5 Execution request

```text
RoomExecutionRequest
  request_id
  room_id
  agent_member_id
  coordinating_execution_id
  target_node_id
  target_resource_ids
  grant_generations
  requested_operation
  capability_budget
  deadline
  cancellation_key
  idempotency_key
```

The target node revalidates all authority locally before acknowledging admission.

## 8. Logical resource namespace

Agents should address resources through stable logical URIs rather than absolute paths:

```text
room://{room}/{node-or-owner}/{resource}/{relative-path}
```

Examples:

```text
room://launch/alice/source/src/main.rs
room://launch/bob/designs/export/icon.svg
room://launch/build-mini/release/
```

The resolver must:

- resolve room, node, and resource from authenticated room state;
- reject unknown, offline, suspended, or stale-generation targets;
- pass only a normalized relative path to the owning node;
- canonicalize again on the owning node;
- reject traversal and symlink escape outside the granted root;
- enforce file type, size, count, and transfer budgets; and
- keep private local paths out of room events and model-visible context.

The namespace creates a unified agent experience. It does not claim that contributed folders are automatically synchronized copies of one filesystem.

## 9. Agent context

A room-authorized agent receives a bounded context assembled from:

- room instructions and policy;
- current human and agent membership;
- selected durable transcript windows;
- room artifacts, decisions, and task state;
- safe node and resource projections;
- the agent's room binding and effective grants; and
- results explicitly returned by resource operations.

Ocean should not inject entire contributed folders into the prompt. Agents inspect them through permission-gated tools on demand.

Room memory is room-scoped by default. Ambient session history, unrelated project memory, and resources from another room must not leak into the execution context.

## 10. Operation modes

### 10.1 Bounded remote file operation

Use for small, specific operations such as listing a directory, reading one file, checking metadata, or applying a reviewed hash-anchored patch.

```text
coordinator
  -> resolve room URI
  -> authenticate target node
  -> submit bounded capability request
  -> target validates grant and permission
  -> target executes locally
  -> target returns structured result
  -> room records audit fact
```

### 10.2 Remote worker near the data

Use for repository analysis, builds, tests, transformations, or other work requiring repeated access to the same resource.

```text
coordinator
  -> request bounded worker on target node
  -> target admits an ordinary permission-gated Ocean execution
  -> worker receives only selected resources and capability budget
  -> worker streams lifecycle events
  -> worker returns patches/artifacts/result envelope
  -> coordinator accepts, rejects, or requests follow-up
```

Moving compute to the data reduces transfer volume and preserves machine-specific environments.

### 10.3 Explicit transfer

Use when the requested result must move between computers. Transfers require their own grant and budget. They are not an accidental side effect of read access.

A transfer should be content-addressed, resumable, integrity-checked, size-bounded, and attributable to a room execution.

## 11. Multi-computer workflow

A coordinating agent may need several resources. The conceptual flow is:

1. Resolve the room task and effective room-agent binding.
2. Select only the resources necessary for the task.
3. Check node availability and grant generations.
4. Produce an operator-visible execution plan when policy requires it.
5. Dispatch bounded operations or workers to owning nodes.
6. Stream durable lifecycle events into the room.
7. Stage writes as patches or artifacts where practical.
8. Apply writes only after target-node permission checks.
9. Aggregate results without widening any worker's authority.
10. Publish a final room artifact with provenance.

Ocean core provides generic authenticated execution, cancellation, capability, event, and permission seams. An extension such as Ocean Crew may own graph construction, worker roles, fan-out, retry, join, reduction, and budget policy. Core must not grow a named-subagent fleet scheduler to implement Rooms.

## 12. Safety invariants

### 12.1 Local custody

The owner node remains authoritative for its files and processes. A room coordinator cannot force execution after the local grant is revoked, expired, unavailable, or denied.

### 12.2 No ambient authority

A remote request runs with the explicit room, agent, resource, and execution grants carried by the request and revalidated locally. It does not inherit the daemon's launch directory, an unrelated session cwd, or all tools available on the target machine.

### 12.3 Fail closed

Unknown room identity, member identity, node identity, resource id, grant generation, capability, or execution scope is a denial. A disconnected coordinator does not turn a bounded request into background unrestricted work.

### 12.4 Generation-safe revocation

Every request carries the resource grant generation and agent-binding revision. Revocation or changed authority invalidates queued and newly arriving requests. Long-running workers receive cancellation and lose permission to begin additional side effects.

### 12.5 Permission-gated side effects

Writes, command execution, process launch, transfers, credential use, and external publication remain subject to target-node runtime policy and operator attention rules.

### 12.6 Path confinement

Filesystem authorization is rooted in a canonical local handle, not a caller-supplied string prefix. Relative paths are normalized and checked after symlink resolution. Resource aliases never become arbitrary absolute-path access.

### 12.7 Secret boundaries

Room context and remote results must exclude provider credentials, environment secrets, private local paths, raw authorization tokens, and unrelated file content. Secret-bearing capabilities require dedicated policy rather than ordinary folder access.

## 13. Concurrency and consistency

Contributed folders remain independently owned filesystems. Ocean should not imply transparent shared-disk semantics.

For writes, use explicit consistency mechanisms:

- expected file digest or version preconditions;
- hash-anchored patches;
- repository branch/worktree isolation;
- short-lived write leases where needed;
- atomic replace on the owner node;
- conflict responses containing safe re-read metadata; and
- durable provenance linking the result to its execution and grant generations.

Cross-node workflows should prefer artifacts, patches, commits, and explicit transfers over simultaneous uncoordinated writes.

A later synchronized workspace product may use Git, an object store, or CRDTs, but that is separate from the first room resource fabric.

## 14. Availability and degraded operation

Resource projections distinguish:

- `available` — the node is authenticated and accepting eligible requests;
- `busy` — online but at its execution or transfer limit;
- `offline` — no current authenticated lease;
- `suspended` — owner or policy has paused the resource;
- `revoked` — the grant no longer exists; and
- `incompatible` — transport or capability protocol versions do not intersect.

A room may queue an intent for later execution only when the owner explicitly allows durable queuing. A queued intent must retain its original agent-binding revision, resource generation, deadline, idempotency key, and capability budget, and must be revalidated before execution.

The UI must not represent `offline` as permission denial or represent `revoked` as a transient connection issue.

## 15. Audit and observability

Every admitted distributed operation should produce fixed-schema facts:

- requesting room and agent member;
- coordinating and remote execution ids;
- target node and resource aliases;
- operation class, not sensitive arguments;
- authority revisions used for admission;
- permission decision and deciding node;
- start, cancellation, completion, and outcome;
- input/output content digests where applicable; and
- resulting patch, artifact, or transfer references.

Room-visible events should explain progress without leaking private paths, prompts, file bodies, command output, or credentials. Detailed local diagnostics remain on the enforcing node under existing logging and redaction policy.

Ocean Observatory may project truthful topology and lifecycle facts. It remains read-only and cannot become the execution or permission authority.

## 16. Surface product model

Rooms should expose three related views.

### 16.1 People and authorized agents

```text
People
  Alice                         owner
  Bob                           member

Authorized agents
  Builder                       mention + tasks
    source: read/write
    release: execute

  Researcher                    mention only
    source: read
    designs: read
```

`Add agent` becomes an authorization flow:

1. browse installed agent candidates;
2. inspect description, model, skills, and requested capabilities;
3. select room activation policy;
4. select eligible resources;
5. review effective read/write/execute authority; and
6. authorize the pinned agent revision.

A bare identifier dropdown is insufficient.

### 16.2 Computers and resources

```text
Computers

● Alice's MacBook
  source          folder       read/write
  references      folder       read-only

● Team Linux
  data            dataset      read/write + execute

○ Build Mac mini — offline
  release         environment  execute
```

`Share local folder` must use a trusted native folder chooser where available. The browser must not submit an arbitrary host path as authority.

The grant flow selects a room-visible alias, access mode, allowed agents, command/transfer policy, approval behavior, and optional expiry.

### 16.3 Work and provenance

Tasks show where work is running and which resources it uses:

```text
Builder — preparing release
  ✓ Alice/source       inspected
  ✓ Team Linux/data    validation passed
  ● Build Mac/release  building
```

The room transcript remains human-readable. Detailed worker graphs and machine events belong in a dedicated projection rather than flooding chat.

## 17. Repository and ownership boundaries

### `ocean-os`

Owns generic device identity, authenticated node transport, resource registration and resolution, local grant enforcement, permission-gated file/execute/transfer capabilities, cancellation, and typed lifecycle events.

It does not own organization-specific agent roles or multi-agent scheduling policy.

### `ocean-surface`

Owns room-agent authorization, computer/resource grant presentation, native folder selection, execution-plan review, permission attention, progress, and provenance UX. It never becomes filesystem or execution authority.

### `ocean-agents`

Owns reusable agent/package conventions and capability requests. Packages request capabilities; they cannot authorize themselves into a room or grant access to resources.

### Orchestration extension / Ocean Crew

Owns worker roles, decomposition, graph policy, lane selection, retry/join/reduce behavior, and bounded distributed workflow semantics over generic host seams.

### Room coordinator / optional Bedrock deployment

Owns authenticated shared room records, safe projections, durable coordination, and team knowledge. It is not a shell, does not possess local folder handles, and cannot bypass node permission authority.

### Tailscale integration

Provides secure peer discovery and direct encrypted reachability. Tailnet identity is an input to Ocean authorization, not a replacement for room membership, agent binding, resource grants, or runtime permissions.

## 18. Relationship to current Rooms

Current Ocean persistent Rooms already provide portions of the control-plane foundation: durable rooms, participant rosters, local agent participants, room-scoped transcript behavior, and independent runtime permission authority.

They do **not** currently provide the architecture described here:

- no multi-node room enrollment contract;
- no room resource registry or logical resource namespace;
- no per-resource room-agent grants;
- no Tailscale-backed resource operation transport;
- no generic distributed worker admission contract;
- no cross-node transfer protocol; and
- no production Surface flow for computers, resources, or agent authorization.

The current single room workspace binding is not equivalent to a distributed resource fabric. It should be treated as an early local compatibility surface, not proof that multi-computer Rooms are implemented.

## 19. Suggested delivery sequence

Each phase requires its own exact implementation manifest and review gates.

### Phase 0 — decisions and threat model

Ratify identity, trust, public/private coordinator behavior, Tailscale assumptions, resource URI format, local custody, revocation semantics, audit redaction, and extension ownership.

### Phase 1 — local room-agent authorization

Replace global-name insertion with pinned room-agent bindings and a real authorization UI. Preserve local-only execution while proving room-scoped context and capability intersection.

### Phase 2 — local resources

Allow one node to contribute bounded folders to a room. Implement native folder selection, safe aliases, local grant enforcement, path confinement, generation-safe revocation, and resource-aware agent tools without networking.

### Phase 3 — node identity and read-only remote resources

Enroll multiple Ocean nodes, bind authenticated Tailscale peers, advertise safe resource projections, and support bounded read/list/stat operations with strict budgets and audit.

### Phase 4 — remote workers and cancellation

Add generic target-node worker admission, lifecycle streaming, cancellation, deadlines, idempotency, and result envelopes. Keep orchestration policy extension-owned.

### Phase 5 — reviewed writes and transfers

Add hash-anchored patches, expected-version writes, explicit content-addressed transfer, conflict handling, and operator approval UX.

### Phase 6 — durable distributed workflows

Integrate extension-owned Crew policy for bounded multi-node fan-out, join, reduction, retries, acceptance, and recovery. Project truthful topology through Rooms and Observatory.

## 20. Open decisions

Before implementation, the operator must decide:

1. whether a room can include nodes outside one tailnet and, if so, which relay protocol is acceptable;
2. whether node enrollment belongs to a human member, an organization, or both;
3. whether agent packages must be installed on every eligible execution node or may be transferred by digest;
4. which resource kinds follow folders in the first public contract;
5. whether read-only remote file operations precede remote workers or ship together;
6. default approval policy for room-triggered reads, writes, commands, and transfers;
7. maximum task, transfer, file, log, and concurrency budgets;
8. offline queue defaults and expiry behavior;
9. how room-agent memory is stored and synchronized without leaking unrelated local memory;
10. whether public Rooms ships a coordinator implementation independent of Bedrock; and
11. how device loss, tailnet removal, member removal, and room revocation invalidate outstanding work.

## 21. Acceptance principles

The architecture is successful when:

- an operator can understand which room agent can access which resource on which computer;
- an authorized agent can complete one task across multiple Ocean computers without manual file shuttling;
- work executes near the owning data and environment;
- every node independently enforces room, agent, resource, and runtime authority;
- revocation stops new side effects across the distributed system;
- no hosted coordinator needs local filesystem credentials or unrestricted shell access;
- results and failures are durable and attributable without exposing sensitive content;
- public Ocean remains usable without private Rising Tides services; and
- distributed orchestration does not create a core daemon fleet scheduler.

The defining principle is simple:

> A Room is a distributed authorization and work context. It lets agents operate across the computers that its members deliberately contribute, while each Ocean node retains custody and final enforcement over its own resources.
