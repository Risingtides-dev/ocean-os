# Ocean Rooms — Gate 0 decisions and threat model

**Date:** 2026-08-17
**Status:** Proposed for operator ruling
**Type:** Architecture decision record and security gate
**Scope:** Resolves the open decisions in `2026-08-16-ocean-rooms-distributed-workspace-architecture.md`
**Implementation authority:** None; acceptance authorizes preparation of a separately reviewed Phase 1 manifest, not code or schema changes

## 1. Purpose

The approved Rooms architecture makes a room a distributed authorization and work context. Members contribute selected folders and compute from their Ocean nodes, and room-authorized agents work across those computers through a logical room namespace.

Gate 0 fixes the trust model and first delivery boundaries before implementation planning. Its central rule is:

> A room can coordinate authority, but every resource-owning Ocean node remains the final authority for its files, processes, credentials, and side effects.

The current implementation remains a local persistent-room system. It has durable rosters, transcripts, artifacts, local/federated access projections, participant ownership records, federation member-to-agent compatibility bindings, and runtime permission gates. Neither current ownership records nor federation bindings are the revisioned `RoomAgentBinding` proposed by the approved architecture. It does not yet have room-scoped agent authorization, room node enrollment, contributed resources, direct Tailscale transport, or distributed execution. No recommendation below may be represented as shipped behavior.

## 2. Fixed invariants

The following invariants are non-negotiable across all phases:

1. **Local custody:** a node never loses final enforcement over its contributed resources.
2. **Explicit room-agent authority:** an installed agent is only a candidate; execution requires an active room-agent binding.
3. **Authority intersection:** effective authority is the intersection of room membership, agent binding, resource grant, node policy, package request, runtime permission policy, and execution budget.
4. **No identity by address:** an IP address, DNS name, tailnet membership, or room display name is never sufficient identity.
5. **No ambient filesystem for resource operations:** from the contributed-resource phase onward, resource-backed requests use room resource identifiers and normalized relative paths, never caller-selected absolute paths. The local-only Phase 1 retains current workspace-binding compatibility and does not represent that compatibility as a resource grant; changing its cwd behavior requires a separate explicit migration decision.
6. **No coordinator shell:** the room coordinator stores safe projections and durable coordination facts but cannot directly exercise local folder handles or unrestricted process authority.
7. **Generation-safe revocation:** stale grants, bindings, and enrollments fail closed.
8. **Permission gates remain mandatory:** distributed execution cannot bypass `ocean-runtime` permission policy.
9. **Content minimization:** room projections and audit events omit private paths, credentials, raw tokens, prompts, file bodies, and command output.
10. **Extension-owned orchestration:** worker roles, graph construction, retries, joins, reductions, and scheduling policy remain outside core.
11. **Public viability:** public Ocean does not require private Rising Tides repositories or a private Bedrock deployment.
12. **Truthful product state:** unavailable, offline, denied, revoked, and unsupported are distinct states in APIs and Surface projections.

## 3. Decisions

### Decision 1 — Initial network boundary

**Ruling:** The first distributed release supports direct operations only between authenticated Ocean nodes that are mutually reachable within one Tailscale tailnet.

- No public inbound daemon exposure is required.
- No generic Internet relay, NAT traversal service, SSH fallback, or arbitrary URL target is included.
- A room may contain members whose nodes are on different networks, but their resources project as `unreachable` until a separately approved transport exists.
- Cross-tailnet federation is deferred. It must not silently proxy file or execution payloads through the room coordinator.

**Rationale:** A same-tailnet boundary provides encrypted peer reachability and an operator-managed device admission layer while Ocean develops its own application identity and authorization. It materially narrows the first threat surface without confusing Tailscale membership with room permission.

**Reversibility:** Additive. A future relay or cross-tailnet transport can implement the same authenticated Ocean resource protocol without changing room resource semantics.

### Decision 2 — Node identity and enrollment

**Ruling:** Every Ocean node has an Ocean device identity distinct from its network address and room member identity.

The minimum enrollment proof binds:

- a stable Ocean `node_id`;
- a non-exportable or owner-protected device signing key;
- the enrolling human room member;
- the room id;
- the observed authenticated Tailscale peer identity;
- a monotonically increasing enrollment generation; and
- an explicit operator approval on the node being enrolled.

A direct request is admitted only when both are true:

1. the transport peer matches the enrolled Tailscale identity; and
2. the request proves possession of the enrolled Ocean device key.

Tailnet IP, MagicDNS name, TLS alone, or possession of a room bearer is insufficient.

Device private keys belong in the platform credential store where available. Exact key algorithms, certificate/envelope formats, rotation, and recovery require independent cryptographic review in the networking phase manifest.

**Rationale:** Tailscale authenticates a network peer, not an Ocean room agent or resource grant. Application-layer identity prevents address reuse, misleading hostnames, and coordinator-only impersonation from becoming execution authority.

**Reversibility:** The identity requirement is fixed; the concrete credential format is replaceable behind versioned enrollment and request envelopes.

### Decision 3 — Node ownership

**Ruling:** V1 enrolls a node under one human room member. That member controls local contribution and revocation.

- Room owners may remove a node's room membership and projections.
- A room owner cannot create or widen a local resource grant on another member's node.
- The node owner can suspend or revoke any local contribution without room-owner approval.
- Organization-owned service nodes are deferred until an organization/service principal and recovery policy are separately defined.
- One human may enroll multiple nodes; each has an independent identity and generation.

**Rationale:** Human ownership gives every local side effect an accountable custodian and avoids inventing organization authority during the first phase.

**Reversibility:** Additive. Service principals can be introduced later without weakening human-owned nodes.

### Decision 4 — Agent installation and revision identity

**Ruling:** An agent must already be installed on every node eligible to execute it, and its package revision or content digest must match the room-agent binding.

- V1 does not transfer or install agent packages between nodes.
- A package name match is not sufficient.
- A changed package digest suspends execution until the room binding is re-authorized.
- Model credentials remain node-local and are never copied through the room.
- A node lacking the pinned package projects the agent as `binding_unavailable` on that node rather than substituting another agent.

**Rationale:** Automatic package transfer combines software supply-chain installation with remote execution and room authorization. Keeping installation local makes the first trust boundary reviewable.

**Reversibility:** A future signed package-distribution system may be added as a separate explicitly approved capability.

### Decision 5 — First resource and compute model

**Ruling:** The first contributed resource kind is a bounded local folder. Compute is not an unrestricted machine capability; it is a bounded worker executed with one or more granted folders as its explicit workspace.

The initial folder grant can independently allow:

- metadata and directory listing;
- file reads;
- reviewed file writes or patch application; and
- worker execution rooted in the resource.

V1 excludes:

- whole-disk or home-directory grants;
- arbitrary host paths nominated by a remote caller;
- raw device, camera, microphone, keychain, browser-profile, Docker socket, or SSH-agent resources;
- privileged execution; and
- a generic `shell on node` resource.

The local grant flow must reject dangerous roots defined by the phase manifest, including the filesystem root and roots that cannot be represented by a stable confined handle.

A worker's cwd is not a security boundary. The remote-worker phase is blocked until its manifest proves OS-enforced process isolation or an equivalently reviewed capability confinement that prevents ordinary process tools from reaching absolute paths, ambient credentials, unrelated processes, and undeclared network targets. If a supported platform cannot provide that boundary, remote workers on that platform may expose only individually confined typed capabilities; ordinary shell/process tools remain unavailable.

The canonical authorization URI shape is:

```text
room://{opaque-room-id}/resources/{opaque-resource-id}/{normalized-relative-path}
```

`resource_id` resolves the owner node through the authenticated room projection; node names and member aliases are display metadata, not authority-bearing URI segments. Friendly Surface labels such as `Alice / source` may be rendered, but callers and durable records use opaque ids. Exact encoding and parser rules belong to the contributed-resource manifest. This ruling refines the illustrative owner/node URI examples in the approved architecture without changing their logical namespace intent.

**Rationale:** A folder is concrete enough to confine and useful enough for cross-computer coding, design, data, and build workflows. Modeling compute as execution near explicit data avoids granting ambient host authority.

**Reversibility:** Additional resource kinds require their own threat model and capability contract.

### Decision 6 — Capability delivery order

**Ruling:** Distributed capabilities ship in increasing side-effect order:

1. local room-agent authorization;
2. local contributed folders;
3. remote read-only `stat`, `list`, and bounded `read`;
4. bounded remote workers with lifecycle and cancellation;
5. reviewed writes and hash-anchored patches; and
6. explicit content-addressed transfers and extension-owned multi-node workflows.

A later stage cannot begin until the preceding stage's revocation, replay, permission, and audit gates pass.

**Rationale:** Read-only resource resolution proves identity, transport, confinement, and availability before remote process and mutation authority exists.

**Reversibility:** Ordering is intentionally hard to reverse; it prevents a broad remote shell from becoming the accidental foundation.

### Decision 7 — Approval defaults

**Ruling:** Creating a resource grant and authorizing an agent are explicit operator actions. Those actions define eligibility but do not automatically approve every side effect.

Initial defaults:

- a room-agent authorization mutation must prove the authorizing local operator principal and room role; neither agent-supplied identity nor an unauthenticated compatibility join/add route may create or widen a binding;
- the Phase 1 manifest must define authenticated caller admission, authorization checks, anti-CSRF behavior for browser-hosted Surface, and a replay-safe explicit approval ceremony bound to the room, agent package digest, requested grants, authorizing principal, expiry, and one-time decision id;
- current local participants and federation member bindings migrate only as `legacy_unverified`/suspended compatibility records until explicitly re-authorized; they do not silently become grants;
- if current Ocean lacks a principal strong enough to prove the authorizing operator, Phase 1 must stop at that gate and introduce or reuse a separately reviewed local-operator authentication seam before binding mutations ship;
- `stat`, bounded `list`, and bounded reads may run without a per-operation prompt only after explicit read authorization and only when the target node's effective runtime policy permits it;
- writes, patch application, command execution, process launch, transfers, credential use, and external publication require target-node permission evaluation;
- the first distributed release defaults those side-effect classes to operator attention even when ordinary local automatic mode would not prompt;
- only the target node's operator may deliberately relax a resource's side-effect attention policy;
- global `skip-all` cannot create room, agent, resource, or node authority that does not exist, but an explicitly configured target node may suppress prompts inside the already intersected grant;
- remote callers and agents cannot change permission policy.

**Rationale:** A room grant is durable authorization to attempt bounded work, not blanket consent to every future command or write.

**Reversibility:** Defaults may become less interruptive after measured use, but authority checks remain fixed.

### Decision 8 — Hard initial budgets

**Ruling:** Every operation carries a deadline and byte/item budget. Initial implementations may lower but not exceed these ceilings without a new reviewed manifest:

- one file read: 8 MiB;
- aggregate read result per operation: 32 MiB;
- one directory listing: 2,000 entries, non-recursive unless a separately bounded walk capability is admitted;
- one patch request: 8 MiB and 128 files;
- one transferred object: 256 MiB;
- aggregate transferred content per execution: 1 GiB;
- retained remote log output per worker: 8 MiB before truncation/artifact spill;
- bounded file operation deadline: 30 seconds;
- remote worker wall time: 30 minutes;
- concurrently admitted remote workers per coordinating execution: 3;
- concurrently admitted room workers per target node: 2; and
- redirect, retry, and recursive delegation: disabled unless the relevant extension budget explicitly permits them.

Content that exceeds a response budget fails with a typed limit result or uses an explicitly admitted artifact/transfer path. It is never silently expanded into the room transcript.

**Rationale:** Fixed ceilings make abuse tests, memory bounds, UI behavior, and cancellation measurable.

**Reversibility:** Values are configuration and protocol-version decisions, but increases require load and abuse evidence.

### Decision 9 — Offline and queued work

**Ruling:** V1 does not durably queue distributed operations for an offline node.

- An unavailable node returns `offline` or `unreachable` before execution admission.
- Coordinators may retain the human's task intent, but it must be explicitly retried after the node becomes available.
- No request executes merely because an old device reconnects.
- Durable offline queues require a separate manifest defining expiry, re-authorization, idempotency, grant generation, package revision, budgets, and user visibility.

**Rationale:** Delayed remote side effects are difficult to reason about and can outlive the context in which they were approved.

**Reversibility:** Additive after durable intent safety is designed.

### Decision 10 — Room memory and context synchronization

**Ruling:** Room transcript, artifacts, decisions, and safe task state are the shared context authority. Local agent memory is not automatically synchronized between nodes.

- Room-agent executions receive bounded room context plus explicit resource results.
- A worker returns a result envelope, patch, or artifact; it does not merge arbitrary local memory into another node.
- Provider caches, local typed memory, ambient session history, and unrelated project context remain local and isolated.
- Shared durable knowledge must be explicitly published as a room artifact or stored through an authorized shared-knowledge service.

**Rationale:** Automatic memory replication risks leaking unrelated local context and creates unresolved conflict and provenance semantics.

**Reversibility:** Typed, provenance-bearing shared memory may be added later as an explicit room resource or Bedrock capability.

### Decision 11 — Public coordinator contract

**Ruling:** The room coordination protocol and a self-hostable public implementation must live in public Ocean-owned repositories. Private Bedrock may implement or extend the contract for authorized deployments but is never required for public Rooms.

The coordinator may hold:

- room/member/node identities and safe projections;
- room-agent binding metadata;
- resource aliases and grant status, never private local roots;
- durable messages, artifacts, task lifecycle, and audit facts; and
- encrypted or opaque routing material required by the reviewed protocol.

The coordinator may not hold reusable local filesystem handles, model credentials, device private keys, unrestricted node bearer tokens, or authority to bypass a target node.

**Rationale:** This preserves public usability and prevents shared coordination from becoming centralized execution custody.

**Reversibility:** Deployment implementations are replaceable if the public wire and trust contract remain stable.

### Decision 12 — Leases, revocation, and device loss

**Ruling:** Distributed execution authority is short-lived and generation-bound.

- Node enrollment, room-agent binding, and resource grant each carry a generation.
- Every request carries the exact generations used for admission.
- The target node validates its local resource generation immediately before each side effect.
- Remote execution authority leases last no more than 60 seconds and require authenticated refresh for longer workers.
- Push cancellation is best effort; lease expiry is the hard partition bound.
- If authority cannot be refreshed, the worker is cancelled and may not begin another side effect.
- Member removal, room revocation, agent removal, resource revocation, package revision change, device-key rotation, or tailnet identity mismatch invalidates affected work.
- Device loss rotates or revokes the enrollment generation and device credential; re-enrollment creates new authority rather than reviving the old node.

The 60-second bound is a maximum stale-authority window under network partition, not a promise that revocation always takes that long. Connected nodes should converge immediately.

**Rationale:** A remote worker cannot safely retain indefinite authority when the coordinator or owner node loses contact.

**Reversibility:** Lease duration may be shortened. Lengthening it requires explicit operator approval and threat analysis.

### Decision 13 — Audit and content boundary

**Ruling:** Every distributed admission and terminal transition produces metadata-only, fixed-cardinality audit facts. Content remains in the room transcript, explicit artifacts, or local diagnostics under their own authority.

Audit includes:

- room, agent member, source node, and target node opaque ids;
- coordinating and remote execution ids;
- resource aliases and grant generations;
- operation class and budget class;
- admission, permission, cancellation, and terminal outcomes;
- start/end times and bounded counters; and
- content digests or artifact references where applicable.

Audit excludes:

- absolute paths and local usernames;
- prompts, thinking, file bodies, patches, command arguments, and command output;
- credentials, environment values, tokens, and tailnet addresses; and
- unbounded or user-controlled metric labels.

**Rationale:** Distributed work must be attributable without making the observability plane a second sensitive-content store.

**Reversibility:** Additional content requires a separately scoped principal and retention policy.

## 4. Threat model

### 4.1 Protected assets

- files and repositories inside and outside contributed roots;
- local credentials, provider secrets, environment values, and platform keychains;
- process execution and machine availability;
- room membership and agent authorization state;
- device identity and resource grant authority;
- private paths, hostnames, usernames, and tailnet topology;
- room transcript, artifacts, and task provenance; and
- integrity of patches, builds, transfers, and published results.

### 4.2 Trust boundaries

1. Human operator to local Surface and daemon.
2. Installed agent package to room-agent binding.
3. Room coordinator to each Ocean node.
4. Tailscale transport peer to Ocean application identity.
5. Coordinating execution to target-node worker.
6. Target worker to contributed resource root.
7. Runtime permission decision to actual side effect.
8. Room-visible metadata to private local diagnostics/content.
9. Core generic execution seams to extension-owned orchestration policy.

Crossing one boundary never implies authority across the next.

### 4.3 Threat actors

- a legitimate but malicious or compromised room member;
- a compromised or adversarial agent package;
- prompt injection contained in a contributed file;
- a compromised room coordinator;
- a compromised peer Ocean node;
- an unauthorized device on the same tailnet;
- a network attacker capable of delay, replay, interruption, or traffic analysis;
- a stale node using revoked membership or grant state;
- a buggy or malicious orchestration extension;
- a curious Surface or observer with metadata-only access; and
- local malware already running as the same OS user.

Local same-user malware is not solved by Rooms. Ocean still minimizes credentials and handles so compromise does not needlessly widen.

### 4.4 Required abuse cases and mitigations

#### Path traversal and symlink escape

**Attack:** A caller submits `../`, absolute paths, symlink swaps, or renamed parents to reach outside the grant.

**Required mitigation:** resource-id resolution, normalized relative paths, owner-node canonicalization, descriptor/handle-relative confinement where supported, point-of-use validation, and fail-closed behavior when stable confinement cannot be proven.

#### Room identity spoofing

**Attack:** A tailnet peer claims another member, node, or agent id.

**Required mitigation:** bind authenticated transport identity to enrolled Ocean device-key possession and a current room enrollment generation. Display names are never admission inputs.

#### Coordinator compromise

**Attack:** The shared service fabricates grants or requests execution.

**Required mitigation:** target-node local grant authority, application-signed request identity, generation validation, runtime permission checks, no private local paths or reusable resource handles in coordinator storage, and short authority leases.

#### Replay and duplicate side effects

**Attack:** A valid write, command, or transfer request is replayed.

**Required mitigation:** request id, idempotency key, target node, grant generations, deadline, operation digest, durable bounded replay ledger for side-effect classes, and exact duplicate response semantics. A different payload under the same idempotency key fails closed.

#### Stale authority after revocation

**Attack:** A disconnected worker continues after member, agent, resource, or device revocation.

**Required mitigation:** generation-bound requests, immediate connected cancellation, target-local checks before side effects, and authority lease expiry within 60 seconds.

#### Agent/package substitution

**Attack:** A node runs a same-named but different agent definition.

**Required mitigation:** bind and attest the package revision/content digest; mismatch projects `binding_unavailable` and admits nothing.

#### Prompt injection from remote files

**Attack:** Content in one member's folder instructs an agent to access or mutate other resources.

**Required mitigation:** data never grants capability. Tool dispatch continues to intersect explicit resource and runtime authority. Cross-resource access appears in the execution plan/audit and prompts according to target policy.

#### Resource and log exfiltration

**Attack:** An agent reads a large tree, encodes secrets in logs, or transfers content through artifacts.

**Required mitigation:** byte/file/depth/deadline budgets, explicit transfer capability, content redaction boundaries, target permission policy, bounded logs, no implicit artifact spill across nodes, and operator-visible destination/resource provenance.

#### Denial of service

**Attack:** A member or extension floods reads, workers, logs, events, or reconnects.

**Required mitigation:** per-node and per-execution concurrency caps, request deadlines, byte/item budgets, bounded queues, backpressure, cancellation, rate accounting without content labels, and no offline execution queue in V1.

#### Confused deputy

**Attack:** A coordinator or remote worker causes a privileged local daemon to use ambient cwd, credentials, or tools.

**Required mitigation:** explicit room/resource context, no daemon-cwd fallback, minimal environment, package/tool capability intersection, target-node permission checks, and no generic remote shell.

#### Malicious orchestration extension

**Attack:** An extension exceeds fan-out, retries indefinitely, changes roles, or broadens grants.

**Required mitigation:** extensions can request only generic host operations; core enforces capability budgets, target grants, cancellation, and permissions. Extension attestations do not grant authority.

#### Information leakage through projections

**Attack:** Room members infer local usernames, paths, addresses, secrets, prompts, or private output from metadata.

**Required mitigation:** allow-listed safe projections, opaque ids, room aliases, fixed status vocabulary, structural redaction, bounded counters, and tests that reject extra serialized fields.

### 4.5 Security non-goals for initial phases

- protecting a contributed folder from malware already controlling its owner account;
- hiding intentionally returned file content from an agent authorized to read it;
- arbitrary public-Internet node reachability;
- transparent shared-disk consistency;
- automatic package distribution;
- privileged or container-orchestrator execution;
- permanent operation during coordinator or tailnet partition;
- organization service accounts and enterprise policy inheritance; and
- perfect traffic-analysis resistance from the tailnet operator.

## 5. Failure semantics

The protocol and Surface must preserve typed failure distinctions:

- `not_authorized` — room/agent/resource authority does not intersect;
- `permission_required` — target operator attention is required;
- `permission_denied` — target operator or policy denied;
- `offline` — enrolled node has no valid availability lease;
- `unreachable` — node is expected online but direct transport failed;
- `stale_generation` — enrollment, binding, or grant changed;
- `binding_unavailable` — pinned agent package is absent or mismatched;
- `budget_exceeded` — bounded request cannot be admitted or completed;
- `conflict` — expected digest/version does not match;
- `cancelled` — authoritative cancellation won;
- `expired` — request or execution authority lease elapsed; and
- `unsupported` — target cannot perform the capability or protocol version.

Clients must not map authorization denial, transport failure, and revocation into one generic disconnected state.

## 6. Source ownership anchors

Current source and contracts that constrain later manifests:

- `crates/ocean-store/src/lib.rs` — current durable room, roster, transcript, access, participant-ownership, federation member-binding, credential, and outbox authority; current ownership/binding rows are compatibility inputs, not the future revisioned `RoomAgentBinding`.
- `crates/ocean-store/AGENTS.md` — credential custody, transactionality, monotonic federation state, and federation binding invariants.
- `crates/ocean-agent/src/lib.rs` and `crates/ocean-agent/AGENTS.md` — product session/history and prompt/context assembly authority that Phase 1 must preserve.
- `crates/ocean-daemon/src/persistent_rooms.rs` — current persistent-room HTTP and local room-agent execution adapters.
- `crates/ocean-daemon/src/room_federation.rs` — current room federation network adaptation.
- `crates/ocean-daemon/AGENTS.md` — daemon execution, permission, session, and extension boundaries.
- `crates/ocean-runtime/AGENTS.md` — mandatory permission, cancellation, cwd, process-tree, and tool-execution contracts.
- `docs/OCEAN_WORKSPACE_BINDING.md` — project/workspace/session cwd authority.
- `docs/OCEAN_PROJECT_MAP.md` — public/private repository and execution ownership.
- `docs/specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md` — generic host seams and extension ownership.
- `docs/specs/2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md` — extension-owned Undertow/Offshore workflow direction.

The existing core Offshore compatibility tools are not the Rooms architecture. A later implementation manifest must either quarantine, adapt through a generic reviewed transport seam, or leave them untouched; it must not silently make them the room scheduler.

## 7. Gate 0 acceptance checklist

Gate 0 is accepted only when the operator explicitly ratifies:

- same-tailnet direct transport as the first network boundary;
- Ocean device identity plus Tailscale peer binding;
- human-owned V1 node enrollment;
- local preinstallation and digest matching for executable agents;
- folder-first resources, the opaque `room://{room}/resources/{resource}/{path}` authorization namespace, and no generic remote shell;
- an OS-enforced process-isolation stop gate before ordinary remote process tools;
- read-before-worker-before-write delivery order;
- authenticated, replay-safe room-agent authorization plus conservative distributed side-effect prompts;
- initial hard budgets;
- no offline execution queue;
- no automatic local-memory synchronization;
- a public self-hostable coordinator contract with optional Bedrock deployment;
- generation-bound authority with a maximum 60-second partition lease;
- metadata-only distributed audit; and
- the threat actors, non-goals, and typed failure vocabulary above.

Acceptance permits drafting the exact Phase 1 implementation manifest for **local room-agent authorization only**. It does not authorize node enrollment, Tailscale transport, contributed folders, remote reads, workers, writes, transfers, schemas, or routes.

## 8. Required Phase 1 manifest boundary

The next manifest must remain local-only and specify:

- the durable room-agent binding owner and migration, with current participant ownership and federation bindings explicitly treated as non-authorizing compatibility inputs;
- the local operator principal, room-role checks, browser anti-CSRF boundary, replay-safe approval decision, and fail-closed stop if authenticated authorizer identity is unavailable;
- pinned agent package identity and revision behavior;
- room role and activation policy;
- room-scoped context and memory boundaries;
- capability request and runtime grant intersection;
- add/inspect/re-authorize/suspend/remove API semantics;
- Surface agent authorization flow replacing the bare global-name picker;
- compatibility behavior for current room agent participants and federation bindings;
- exact permission and attribution tests;
- migration rollback and downgrade behavior; and
- cross-repository validation and rollout gates.

It must not include Tailscale, node enrollment, resource grants, remote execution, package transfer, worker graphs, or multi-node scheduling.
