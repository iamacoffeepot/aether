# ADR-0149: Bloomery — bounded development blooms on a first-party control plane

- **Status:** Proposed
- **Date:** 2026-07-15

## Context

ADR-0146 put the autonomous pipeline's orchestration on GitHub Actions and rejected an external orchestrator
service because it added "an operational surface with no compensating capability at this scale." That
cost judgment was correct when written and the pipeline it produced works: the phase ladder, the reconciler,
the wavefront dispatcher, and the native review gate (ADR-0148) move real issues from sketch to merge with
little human attention. But operating that pipeline has surfaced three forces the original decision did not
price, and together they are the compensating capability.

**Security.** GitHub has accumulated four unrelated roles: social intake, canonical workflow state, executor,
and receipt surface. The dangerous combination is the middle two — workflows that make state-transition
decisions while holding secrets, triggered by events that untrusted parties can influence. The posture is
defensible only through constant vigilance (sender gates on every secret-reaching path, least-privilege job
splits, actor re-verification on labels), and vigilance is exactly what an autonomous fleet erodes. The
control plane and the execution of untrusted source share a security domain by construction, and no amount
of workflow hardening separates them.

**Testability.** The pipeline's logic lives in event choreography across seven workflow files, and event
choreography cannot be run on a bench. Its failure modes are discovered live: a status posted after a write
made a gate fail open, a human-shaped identity assumption made bot-authored PRs invisible to an operator tool
(#3445), a merge/label race left Done cleanup to a component that had already exited (#3446), and a polling
contract pinned the wrong SHA across a fix push (#3448). Each fix was small; the class is structural. State
transitions that live in shell steps triggered by webhooks are not property-testable, not replayable, and
not observable except by archaeology.

**Boundedness.** The board-wide continuous scheduler cannot name the set of work it has promised to finish.
There is no unit that freezes membership, scope, and source head; forecasts what the set will cost; executes
against exactly that promise; and reports actual against forecast. Without that unit the computation surface
is unbounded and unauditable — resources cannot be predetermined for a set, and study has no fixed promise
to grade against.

Separately, the substrate has matured into a general application host: typed actor components on a chassis
with narrow native capabilities, content-addressed artifact stores, HTTP ingress, process supervision, and
two test harnesses. The engine is now capable of hosting the pipeline as a first-party application — and a
demanding non-game application is exactly the dogfood the engine needs.

## Decision

Build **Bloomery**: a public, first-party Aether application that owns the software-development assembly
line, with GitHub demoted to one operator-installed plugin. This supersedes ADR-0146's architectural
placement — the "external orchestrator" its alternatives rejected is now the decision, hosted on Aether
rather than bought — while ADR-0146 remains the operating decision for the current pipeline throughout the
migration, and its gate semantics (owner approval tiers, the ADR-routing rule, kill switches) carry forward
as Bloomery policy rather than being relitigated.

### The bloom

Bloomery's unit of work is the **bloom**: a bounded, immutable source transaction. Its lifecycle is one-way:

```text
BloomDraft        mutable shaping — membership proposals, dependency graph, forecast;
                  drafts overlap harmlessly and claim nothing
  --seal-->
BloomSpec         immutable: sorted workpiece scope-revision digests, one base source
                  digest, stage-catalog / toolchain / policy digests, budget, forecast;
                  the bloom id is the digest of the canonical spec bytes
  --execute-->
ResolvedBloom     one artifact: final tree, integration lineage, and a resolution claim
                  plus evidence for every member workpiece
  --compare-and-swap land-->
LandingReceipt    mainline moved from B0 to B1; the next bloom seals on this receipt
```

A **workpiece** is the stable identity of one intended change; a GitHub issue is one projection of it, and
an umbrella issue is a collection, not admissible directly. Sealing is one serializable store transaction:
verify every member's scope and approval lineage, insert the complete membership set under a uniqueness
constraint of at most one active bloom per workpiece, and commit — or abort whole, naming the conflict. A
sealed spec never amends. Changed membership, scope, base, or policy creates a **successor bloom** that
atomically inherits the predecessor's claims and names it superseded.

Workpieces integrate onto a Bloomery-owned branch hierarchy (a single-writer integration branch per bloom,
ephemeral per-attempt work branches) but do not land independently. Aggregate verification and review run
once on the final tree, and landing is a compare-and-swap: if mainline is no longer the sealed base, the
bloom is not rebased under its evidence — a successor seals on the new head and reuses every candidate and
checkpoint the drift did not invalidate. V1 permits one sealed, unlanded bloom per mainline; blooms chain
strictly through landing receipts. Speculative sealing against an unlanded predecessor's resolved artifact
is a named future mode, not part of this decision. After landing, a study report grades actual cost, time,
retries, and interventions against the sealed forecast — the bounded promise is what makes the grade mean
something.

### The value vocabulary

Everything durable is an immutable **artifact**, typed content addressed by digest, forming a derivation
DAG in which every artifact names its parents. **Statements** are artifacts carrying words plus one of three
provenance claims: an *author signature* (a person asserted these exact bytes for this purpose — the only
claim that can become instruction), an *observation attestation* (an adapter saw these bytes elsewhere —
context, never command), and a *stage receipt* (a configured agent profile ran one process over exact inputs
and produced exact outputs). **Evidence** — approvals, verification results, review findings, resolution
claims — binds to exact digests; refinement produces a new candidate and old evidence never validates the
replacement. Every model call consumes a **prompt manifest** listing each slot by artifact digest, role, and
parent closure, and assembly is fail-closed: an instruction-capable slot that does not trace to a signed
statement or a versioned policy artifact rejects the attempt before dispatch.

The signature *mechanism* is deliberately stubbed in v1. The statement, manifest, and receipt shapes ship
from the start — everything downstream binds to them, and they are what make replay and audit possible —
but with a single operator there is no second signer to defend against, so envelopes verify against a fake
key provider until real key custody (and its rotation, revocation, and signing-console surface) is a
separate, later arc. The fail-closed prompt closure is structural and is enforced from day one regardless.

### The line

The pipeline is a closed stage vocabulary compiled into Rust — sketch, scope, approve, construct, verify,
refine, review, integrate, aggregate verify/review, land, study — not a workflow language. A **stage
binding** declares the artifact kinds one stage consumes and produces, the agent profile that runs it
(`iama-{stage}`, an attempt-scoped worker identity, never a resident actor or a delegable authority), the
skill or process it executes, its completion gate, and its retry budget. The full catalog is itself a digest
the bloom freezes at seal. An **attempt** executes one binding against one subject; agents return proposed
artifacts and evidence only — the reducer alone advances state. A **transformation** is the portable unit of
execution: a typed command (`verify.clippy`, `construct.implement`) with declared inputs, outputs, image,
limits, and network profile, invoked identically on a laptop, on Actions, or in an isolated worker.

### The control core

One pure function — `reduce(snapshot, event) -> decisions` — owns every state transition. Events are
admitted facts with idempotency keys; decisions are value objects entering a transactional outbox; side
effects never occur inside the reducer. The journal plus the content-addressed artifact bytes are the only
truth. The first store is SQLite in WAL mode behind a `store` port owned by a native capability (a new
`rusqlite` dependency, host crate only); the active-membership uniqueness constraint lives there, so bloom
exclusivity holds even with every plugin offline. Recovery is journal replay plus outbox republish. The wasm
application's actors — a supervisor, one coordinator per live bloom, a projection coordinator — hold nothing
but rebuildable projections; killing the process loses no state.

### The boundary

Six typed ports, each owned by a native capability so the wasm application never touches keys, databases,
tokens, or shells:

- **store** — journal transactions, membership claims, inbox/outbox
- **artifacts** — digest-addressed bytes; canonical record, never evicted
- **source** — snapshot, checkpoint (produce *and enumerate*: successor reuse requires checkpoints be
  queryable by digest, so enumeration is part of the port contract, not an adapter nicety — *amended
  2026-07-15, making explicit what the successor-reuse clause already implied*), integrate,
  compare-and-swap land; branch names are working handles, never identity
- **executor** — `submit` / `cancel` / `inspect` / `stream_evidence` to disposable workers; no
  arbitrary-command message exists
- **signing** — verify statements, sign receipts; private keys never enter wasm
- **projection** — push typed receipts outward

The artifacts capability reuses the engine store's storage layer rather than growing a rival. The
content-addressed core inside the hub's binary/component store (ADR-0115/0116) — sha256 addressing, atomic
sidecar writes, index restore, pid-locking — is domain-clean and tested, but its public entry type is a
closed binary/component manifest enum and its disk-budget LRU eviction is cache semantics: correct for
re-uploadable binaries, destructive for a canonical record (pins are not yet persisted across restarts, and
unnamed entries evict silently). A prerequisite refactor extracts that core with entry metadata and eviction
policy as parameters; the hub store keeps its current behavior as one consumer, and Bloomery's instance runs
eviction-free with provenance kept in the journal — avoiding the "two stores duplicate addressing and
eviction" outcome ADR-0116 rejected.

GitHub lives entirely outside those ports as `aether-bloomery-github`, and its direction is outward
*(amended 2026-07-15: the original text made the adapter an intent importer — issues becoming sketch
candidates, reviews becoming decision proposals; the owner's intent is the reverse)*: the adapter maintains
a shadow copy of Bloomery's internals — workpieces project to issues, blooms to their aggregate views,
evidence to checks and comments, every projection carrying internal ids and digests in stable metadata,
idempotent and rebuildable from the journal after deletion — and it implements the source port when GitHub
hosts the repository, so Git remains the versioning substrate. Intent enters Bloomery natively; GitHub is
never the driver. Two narrow inward channels exist. First, stage results: when a stage Bloomery dispatched
executes on GitHub — a reviewer verdict, a check run — the adapter normalizes the outcome into evidence
bound to the exact digests Bloomery displayed, entering the reducer like any other attempt result. Second,
observation intake *(second amendment 2026-07-15: importing material is allowed; driving is not)*: the
adapter may import a selected issue or comment as an observation-attested artifact — material a bloom
draft can be shaped from — but an observation carries no authority: it becomes intent only when a person
adopts its exact digest in a native statement, and no platform event advances the reducer by itself.
Platform authentication is never an author signature, a comment never becomes a command, and an
unrecognized webhook at most flags a drifted projection for repair. No core module names a GitHub type.
Unplug the adapter and an active bloom still runs to completion — the mirror lags and rebuilds.

### Execution on Actions, by demotion

GitHub Actions remains the first executor backend — and a permanent one for the lanes it suits — with
exactly one role: a worker pool. The executor port dispatches a fully resolved work order (transformation
id, digest-pinned inputs, declared outputs, idempotency nonce) via `workflow_dispatch` to a thin wrapper
pinned at a protected ref; the wrapper checks out the exact digest, runs the same `cargo xtask` entrypoint a
contributor runs locally, and uploads evidence bytes the broker accepts only when nonce and digests match.
No event triggers steer anything and no decision logic remains in YAML. Security then splits by lane:
untrusted lanes (imported source, model-generated candidates) run zero-secret on ephemeral hosted runners,
where a fully compromised job can produce lying evidence — an untrusted claim the reducer validates and can
re-check — but cannot advance state, land, or reach a key, because the dangerous authorities (mainline push,
App private keys, receipt signing, the journal) move into the Bloomery host's native capabilities and leave
Actions entirely. What Actions cannot provide — egress control, enforcement that wrappers stay thin,
sub-minute latency — is the eventual case for an ephemeral-VM broker backend behind the same four-message
port: a backend swap, deferred until one of those limits bites.

### Packaging

Three public crates in the Aether workspace, following the `aether-kit` rlib/cdylib precedent:

```text
aether-bloomery         canonical values + pure reducer + wasm actors (rlib/cdylib)
aether-bloomery-host    BloomeryChassis + native capabilities + API/console + bloomery bin
aether-bloomery-github  GitHub intake/source/projection adapter, statically linked by the host
```

No kinds crate (mail shapes live with their owning modules), no separate CLI or console crate (both ship
with the host), no plugin SDK or dynamic ABI (v1 statically links reviewed adapters). A new crate appears
only when an integration brings a real dependency, release, or isolation boundary. Bloomery-specific
concepts stay in Bloomery; a primitive moves down into `aether-actor` / `aether-substrate` /
`aether-capabilities` only once proven useful outside this application. The dedicated `BloomeryChassis`
hosts the capabilities and autoloads the policy component; the generic hub and headless chassis do not
become a build server.

### Migration

Each step is reversible and the current pipeline keeps operating until explicitly retired:

1. **Mirror.** `aether-bloomery` values + reducer + property tests; the storage-core extraction; the
   SQLite store capability; and the projection mirror — synthetic blooms driven through the reducer and
   journal appear as carbon copies on GitHub, idempotently, rebuilt after deletion. No execution, no
   landing authority; the gate to step 2 is a faithful mirror plus kill-and-restart drills converging.
   *(Amended 2026-07-15: originally a read-only importer predicting the live board's decisions — that
   pointed the shadow inward; the mirror points it outward, matching the adapter's direction above.)*
2. **Executor bridge.** Bloomery seals real blooms and dispatches work through the Actions wrapper lane;
   the tick's scheduling retires. GitHub objects remain the social view; landing authority is unchanged.
3. **Authority moves.** Compare-and-swap landing moves to the source port; App keys and receipt signing
   move into host capabilities; workflow secrets ratchet down to zero-secret lanes. ADR-0146's machinery
   retires stage by stage as each authority moves; ADR-0148's native review gate remains as
   defense-in-depth around the GitHub-hosted mainline, no longer the gate of record.
4. **Later, on demonstrated pressure:** the ephemeral-VM worker broker, real signing custody, speculative
   successor blooms, additional forge/source plugins.

## Consequences

- The control plane becomes property-testable pure Rust with a replayable journal; the event-choreography
  defect class (#3445, #3446, #3448) ceases to exist as a class because no state transition lives in a
  workflow file.
- Secrets and untrusted execution stop sharing a security domain. The worst credential in any workflow
  becomes a scoped short-lived token; a compromised worker degrades to lying evidence, which is validated
  like all evidence.
- Work acquires a bounded, auditable unit: a sealed bloom predetermines its resources, names its complete
  promise, lands atomically or supersedes explicitly, and is graded against its own forecast.
- Continuous per-PR landing goes away: nothing reaches mainline until its bloom resolves, a stuck workpiece
  is ejected only by sealing a successor, and a mid-bloom hotfix forces supersession on the new head. These
  costs are accepted knowingly in exchange for the bounded audit surface; bloom sizing (small, frequent
  blooms) is the operational mitigation.
- Aether gains its first demanding public non-game application, exercising actors, capabilities, stores,
  HTTP, and both test harnesses as a production consumer — and its development remains on the current
  pipeline, which builds its successor.
- New surface to own: a public protocol schema (versioned before 1.0 with low reversal cost, expensive
  after), the host service's operational story, and a `rusqlite` dependency.
- Follow-on ADRs this creates: the worker broker when it becomes real, signing custody when a second signer
  exists, and the speculative-bloom mode if serial throughput ever binds. ADR-0146 flips to superseded (in
  its orchestration placement) only when step 3 completes; its gate policy lives on as Bloomery policy.

## Alternatives considered

- **Harden Actions in place** — lowest cost, and it is what the last month of CI fixes has been doing;
  rejected because the defect class is structural: choreography stays untestable, secrets stay co-resident
  with untrusted input, and no bounded unit of promise exists.
- **Per-workpiece landing under a bloom used only for admission and forecasting** — keeps today's
  continuous throughput and most of the audit value; rejected because the aggregate promise then has no
  artifact — members land beneath it and the "one resolving artifact per sealed set" property, which is the
  boundedness rationale itself, is lost. Revisit if serial blooms bind in practice.
- **Buy the orchestrator** (Temporal, Restate, Buildkite, Argo) — real durability and executor maturity at
  low build cost; rejected because the valuable semantics — blooms, workpieces, evidence, receipts — would
  still need building above the vendor, which adds a dependency without removing the work, and the engine
  loses its dogfood.
- **A generic workflow/BPMN engine or a GitHub-Actions-compatible interpreter** — maximum flexibility;
  rejected for far higher build and cognitive cost than directly encoding the one development line this
  repository actually runs, and the repository has already retired one bespoke YAML execution grammar in
  favor of typed Rust (ADR-0067's history).
- **Move only scheduling off GitHub, keep GitHub objects canonical** — a reversible bridge, and roughly
  what migration step 2 passes through; rejected as a destination because issues, labels, and runs remain
  the database and every defect class above survives.
- **One shared mega-branch per bloom, hand-managed** — lower implementation cost; rejected for shared-writer
  races and unrecoverable provenance; the owned integration branch plus per-attempt sub-branches gives the
  same aggregate boundary with one writer.
