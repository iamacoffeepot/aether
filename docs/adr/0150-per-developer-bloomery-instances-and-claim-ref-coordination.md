# ADR-0150: Per-developer Bloomery instances and claim-ref coordination

- **Status:** Proposed
- **Date:** 2026-07-16

## Context

ADR-0149 defines Bloomery with an implicit topology: one instance per mainline. Both of its exclusivity
mechanisms are instance-local — the journal is "the only truth," and "at most one active bloom per
workpiece" is enforced by the SQLite store's active-membership uniqueness constraint
(`crates/aether-bloomery-host/src/store/runtime.rs`). That holds only while exactly one store exists.

The intended use is wider: Bloomery is the build system each team member runs, not a service one operator
hosts. Each developer runs the full control plane on their own machine, and the model-driven construct
lane executes Claude Code headless *on that machine with that developer's own subscription*. This is not
just ergonomics — subscription OAuth tokens are year-long, inference-scoped, non-delegable credentials
that Anthropic's terms restrict to the holder's own Claude Code use, so a shared runner executing on
someone else's subscription is not a buildable or permissible design. The heavy transformations
(compile, test, verify) dispatch to shared cloud runners on digest-pinned work orders, so a member's
laptop is never throttled by builds. Warm-session reuse (model-keyed sessions, subsystem-keyed leases,
effort pinned per session because an effort flip invalidates the prompt cache) is per-instance equipment
of the construct lane: every cache miss burns that member's own quota.

With several instances working one repository, whatever cannot stay instance-local needs a shared
mechanism. Landing already federates safely: it is a compare-and-swap on the mainline ref, Git is the
shared authority, and a losing bloom gets a clean `BaseMoved` and seals a successor. Workpiece claims do
not: two members' instances can seal overlapping blooms with neither store able to see the other's claim.

## Decision

**Bloomery is a per-developer application.** One binary, one instance per developer; the repository's
owner runs an instance like everyone else's (cloud-hosted or local — no instance is architecturally
special, and running the shared runner pool confers no extra authority over anyone's bloom).

**Execution lanes are part of a work order's identity.** A transformation declares its lane, and the
evidence broker enforces it. Construct-lane (model-driven) orders execute on the instance owner's own
harness with their own credential; heavy lanes execute on the shared cloud runner pool. Verify evidence
is accepted only from the trusted runner lane, matched by nonce to an order the coordinator dispatched
there — a worker cannot verify its own candidate locally and self-report green. Credentials never leave
the machine they live on; no coordinator holds one. A contributor without push access routes candidate
trees through their fork, exactly as pull requests do today.

**The claim registry moves to the shared repository as refs.** Claiming a workpiece for a sealing bloom
is an atomic ref creation via the Git Data surface (the source client already carries `create_ref`):
the ref existing *is* the claim, and a seal whose member is already claimed aborts whole, naming the
conflict — the same failure shape ADR-0149's store constraint produces today. ADR-0149's one sealed,
unlanded bloom per mainline is enforced the same way: sealing takes a single mainline-admission ref,
released by the landing receipt or an explicit supersession. When a supersession seals a successor, the
predecessor's refs **transfer** rather than release: the mainline-admission ref and every carried-over
workpiece ref are compare-and-swapped from the predecessor's value to the successor's (a concurrent
mutation loses cleanly), fresh refs are created only for the successor's net-new workpieces, and plain
release applies only to members the successor drops — release-on-supersession without a successor is
the abandonment case only *(amended 2026-07-16: the original sentence read as release-on-supersession
unconditionally, which frees the admission ref while the successor is a sealed, unlanded bloom and lets
a second instance seal against the same mainline — found by the critic pass on the claim-coordination
slice, PR #3529)*. Claim refs are working handles carrying the
claiming bloom id; release, transfer, and supersession are recorded with the bloom's receipts, so the ref
namespace is auditable against journals. The local store's uniqueness constraint remains as each instance's
backstop; the ref namespace is the inter-instance truth for claims — the one datum whose truth
deliberately lives outside the instance journal.

An instance heals **its own** interrupted operations at boot — a heal is in scope exactly when the
instance's journal proves ownership of every ref it touches *(amended 2026-07-17: drawn while scoping
the deep-heal slice, #3555)*. The claim-registry port grows two ops for this: claim-ref **enumeration**
(the Git Data surface already lists refs by prefix) and an **idempotent per-ref transfer completion**
that treats a ref already at the successor's value as a no-op, so re-driving an interrupted supersede
converges without changing the transfer op's landed CAS semantics. On these, boot reconcile sweeps a
tombstoned ref an interrupted release left behind and completes a half-transferred supersede its own
journal records. Reclaiming an **own orphan** — a ref this instance created but whose admit never
reached its journal — needs a write-ahead intent record in the seal-admit choreography and is a named
follow-on, deliberately not folded into this amendment: a change to the durable admission sequence is
decided on its own, not as a rider. Foreign refs are never healed, only reported: the dead-instance
staleness boundary below stands.

**A queryable coordination service is a named follow-on, not part of this decision.** A thin claims +
bloom-admission service (which a cloud-hosted instance could host) buys "who holds what, which bloom
lands next" visibility that ref enumeration answers poorly, at the cost of a new operational surface and
an availability dependency in the seal path. It is deferred until team scale makes that visibility worth
the surface, behind the same claim semantics — a backend swap, mirroring ADR-0149's worker-broker
deferral.

## Consequences

- Team members reuse the same assembly line the owner runs, locally, on their own subscriptions. No
  shared-credential surface exists anywhere in the system, by construction rather than by policy.
- The seal path gains a dependency on the Git host's availability — the same dependency landing already
  has, now at admission time too. An instance can still draft and shape offline; it cannot seal.
- Dead instances can strand claims: a crashed laptop's bloom holds its workpieces until its owner (or an
  operator) releases the refs. Claims carry the claiming bloom id, so staleness is auditable but not
  self-healing; liveness/expiry is the first concrete pressure that justifies the follow-on service.
- ADR-0149 remains the governing decision for everything inside an instance; this ADR revises its
  implicit single-instance topology, demotes the store constraint to an instance-local backstop, and
  scopes "the journal is the only truth" to per-instance state.
- The session/warm-pool machinery becomes a required deliverable of the construct lane on every
  instance, not an optimization of a shared pool.

## Alternatives considered

- **A coordination service now** — queryable claims and landing order from day one; rejected as the
  starting point for adding an operational surface and a seal-path availability dependency before team
  scale demands it. Named as the follow-on; the claim semantics here are designed so it slots in as a
  backend swap.
- **One shared cloud Bloomery, members as remote workers** — centralizes coordination trivially;
  rejected because the construct lane on shared infrastructure either shares subscription credentials
  (prohibited) or forces every member onto metered API billing, and it makes one instance
  architecturally special, which the per-developer product intent rejects.
- **Optimistic sealing with no shared claims** — instances seal freely and landing's CAS sorts winners;
  rejected because overlapping blooms burn real execution on work only one bloom can land, and a
  workpiece resolved under two blooms breaks the bounded-promise accounting ADR-0149 exists to provide.
- **Status quo (instance-local claims, human coordination)** — rejected: silent overlap between two
  members' blooms is exactly the unbounded, unauditable coordination failure the bloom unit was created
  to eliminate.
