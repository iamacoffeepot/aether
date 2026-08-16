# ADR-0149: Bloomery — bounded development blooms on a first-party control plane

- **Status:** Accepted (shipped — bounded Bloomery state transitions in `crates/aether-bloomery/src/reduce/` and the control surface in `crates/aether-bloomery/src/control/`)
- **Date:** 2026-07-15
- **Amended:** 2026-08-11 (#4663) — the outward projection targets the objects a repository already holds instead of opening its own. See _[2026-08-11 amendment (#4663): the projection targets existing objects](#2026-08-11-amendment-4663-the-projection-targets-existing-objects)_; §The boundary's projection clause reads under it.
- **Amended:** 2026-08-16 — the projection may create and fully own the issues it creates, and still never writes an object it did not create. See _[2026-08-16 amendment: the projection owns what it creates](#2026-08-16-amendment-the-projection-owns-what-it-creates)_; the 2026-08-11 §The write surface reads under it.

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

**Amended by ADR-0182 (what a signature covers).** An author signature does not cover the statement's words
alone. It covers the digest of a typed authorization — the door it authorizes, the exact request digest it is
bound to, and the words together — so one signature authorizes one request at one door. `parents` keeps the
derivation-DAG meaning above and stays outside the signature, which is precisely why binding a door to it
structurally was not enough: a holder can rewrite that field without disturbing the signature. See ADR-0182
for the door enum, the migration, and the alternatives weighed.

### The line

The pipeline is a closed stage vocabulary compiled into Rust — sketch, scope, approve, construct, verify,
refine, review, integrate, aggregate verify/review, land, study — not a workflow language. A **stage
binding** declares the artifact kinds one stage consumes and produces, the agent profile that runs it (a
digest-addressed, versioned `AgentProfile` artifact — model, reasoning effort, tool policy — referenced by
digest, so a receipt attests the exact configuration that ran; the `iama-{stage}` worker identity, an
attempt-scoped identity never a resident actor or a delegable authority, is *derived from the stage*, not
stored), the skill or process it executes, its completion gate, and its retry budget. The full catalog is itself a digest
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
- **projection** — push typed receipts and self-contained view documents outward: a view document is a
  pure projection of the journal carrying everything an adapter renders (per-bloom membership, scope
  revisions, stages, evidence digests, resolution claims) — an adapter never queries back into the store
  *(amended 2026-07-15: the first port draft carried opaque ids only, which no adapter can render; found
  by the judge pass on the projection-mirror slice, twin of the checkpoint-enumeration amendment)*

*Amended 2026-07-19 (the wasm-boundary retirement):* the six ports above stay native-owned, but the
control core they front — the single-writer reducer-owner — is no longer a wasm component. The sandbox was
meant to keep control logic from touching keys, the database, or a shell; but a sandbox is only a boundary
across a *trust asymmetry*, and the operator controls the host binary and the control logic alike — one trust
domain, no asymmetry. So the wasm line guarded a door in a field while forcing every native peer to address
the core by mailbox rather than by type (the friction that surfaced it, #3672/#3684). The control core is now
a native capability (`aether-bloomery-host`'s `control`, a `#[actor(singleton)]` beside the store / signing /
artifacts / source caps it already drove): `reduce()` links directly and the api and reactors address it as a
typed peer. wasm stays reserved for genuine *extension* surfaces — user- or agent-authored logic on a fixed,
trusted host — which the bounded control reducer is not. The typed-port boundary itself is unchanged; only
the thing behind it moved from a sandbox to a sibling.

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
idempotent and rebuildable from the journal after deletion *(amended 2026-08-11, #4663: what a projection
**owns** is narrowed to marker-keyed comments on objects the repository already holds; see the amendment
section)* — and it implements the source port when GitHub
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
aether-bloomery         canonical values + pure reducer + control/source mail kinds (rlib leaf)
aether-bloomery-host    BloomeryChassis + native capabilities (incl. the control core) + API/console + bin
aether-bloomery-github  GitHub intake/source/projection adapter, statically linked by the host
```

*(amended 2026-07-19: `aether-bloomery` was formerly also a `cdylib` compiling the control-core wasm actor;
with the control core native (§The boundary, amended), it is a pure rlib — no wasm component, no guest-SDK
dependency — that the host and the GitHub adapter both link inward, cycle-free.)*

No kinds crate (mail shapes live with their owning modules), no separate CLI or console crate (both ship
with the host), no plugin SDK or dynamic ABI (v1 statically links reviewed adapters). A new crate appears
only when an integration brings a real dependency, release, or isolation boundary. Bloomery-specific
concepts stay in Bloomery; a primitive moves down into `aether-actor` / `aether-substrate` /
`aether-capabilities` only once proven useful outside this application. The dedicated `BloomeryChassis`
mounts the capabilities — the control core among them, now native (§The boundary, amended) rather than an
autoloaded wasm component; the generic hub and headless chassis do not become a build server.

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

## 2026-08-11 amendment (#4663): the projection targets existing objects

§The boundary's projection clause is written for a topology where the mirror is the only inhabitant of the
repository it writes into — a dedicated shadow repository, where "workpieces project to issues, blooms to
their aggregate views" describes objects nothing else could be confused with. Pointed at the repository
Bloomery develops, the same clause writes duplicates of objects that are already there. A workpiece named
`issue-4628` names an issue that exists, and the adapter opens a second one titled `Workpiece issue-4628`
beside it, with a `Bloom <hex>` umbrella above that. Neither is actionable: nothing is assigned to them, no
reducer transition reads them, and nothing closes them — the projection's issue verbs are create and
overwrite-title-and-body, and no close verb exists. Their bodies render state whose source of truth is the
journal, which the control API serves live at `GET /view`. A machine-generated `Bloom <hex>` title cannot
satisfy the repository's Conventional-Commits title lint either, so every umbrella is marked invalid by
construction. Finding those objects by marker also scans repository-wide issue history before every write —
dozens of sequential list requests per lookup on a repository of any age, growing without bound, against a
page cap that eventually reports a present object as absent.

This amendment narrows what a projection **owns**. It does not change the direction of the boundary, the
self-contained view document, or the rule that a platform event never advances the reducer.

### Addressing

The GitHub adapter resolves a `WorkpieceId` to an existing object when, and only when, the id is exactly
`issue-<N>` with `<N>` a canonical decimal — non-zero, no leading zeros, no sign, no surrounding space —
naming that object's number in the repository the adapter is already configured for. `WorkpieceId` keeps the
identity §The bloom gives it: an arbitrary native handle no platform owns. The convention is adapter-local;
the core gains no issue semantics and answers no question about numbers.

Any other id shape is **unaddressable on GitHub**, which is an ordinary state rather than a fault. A
workpiece with no GitHub home has no comment projected and keeps `GET /view` as its authoritative view.

The number resolves against whatever the configured repository holds:

- **Closed** is a target like any other. Closure is the ordinary terminal state of a workpiece that landed,
  and a receipt on a closed issue is the receipt where it belongs. The projection never opens, closes, or
  reopens anything, so a closed target stays closed.
- **A pull request number** is a target too. GitHub numbers issues and pull requests from one sequence and
  the comment route is shared, so refusing would strand a workpiece whose source is a pull request and buy
  no safety.
- **Absent, or locked against comment**, is reported and skipped. Nothing is fabricated to give the
  projection a home — that fabrication is the behavior this amendment removes.

Nothing verifies that the configured repository is the one the ids were authored against, because there is
nothing trustworthy to verify against: number 4628 is a live issue in most repositories of any age. The risk
is answered by bounding the write rather than by an unavailable assertion, and bounded further by the fact
that this same configuration is where the source port pushes refs and opens landing pull requests — a wrong
repository breaks landing loudly, before a projection can write anywhere quietly.

### The write surface

A projection may create and update **only comments carrying its own marker**, and only on objects it did not
create. It may not write an issue or pull-request title or body. It may not open, close, reopen, lock,
label, assign, or merge. The client contract loses its issue create, overwrite, and find-by-marker verbs, so
no method reachable from a projection can address a human-authored body — the bound holds by absence rather
than by discipline. *(Amended 2026-08-16: a projection may also create issues and own the title and body of
issues it created; the prohibition on writing an object it did not create is unchanged. See the 2026-08-16
amendment section.)*

A comment is the surface, rather than a marker-delimited managed region inside the body, because a managed
region still writes the whole body back. Every update would round-trip a person's prose through the
projection's renderer, and one marker mis-parse, one truncated read, or one human edit landing between the
read and the write replaces an authored body with a machine render — through the API, with no undo. A
comment carries the same content with none of that exposure: a separate object, additive, individually
deletable, worst case noise. A label carries no content, and the payload here is rendered prose. Writing
nothing at all is insufficient for the same reason ADR-0151 gives for held questions: the receipt and the
parked question have to be visible where a person already looks, and the umbrella was the exception to that
principle rather than an instance of it.

### What each object is

A **member** projects as one comment on its source issue, keyed by workpiece *and* bloom and digested over
the whole member render. State, approval, resolution, and any held question fold into that single comment
rather than taking one apiece: they derive from the same value and change together. The bloom half of the
key is load-bearing — a successor bloom re-admitting the same workpiece now shares one issue with its
predecessor, and a workpiece-only key would have the two overwrite each other.

A **bloom** has no object of its own before it lands. Afterwards its landing pull request is the aggregate:
one per bloom by the landing-branch convention, carrying the whole diff, closing itself on merge. No
umbrella issue is opened at any point.

A **landing receipt** projects as one comment per bloom onto every resolvable member issue, and onto the
landing pull request when one exists. The pull request is a target, not a precondition — a bloom can land
through a path that opened none, and requiring it would wedge those lanes.

### The receipt carries its members

`LandingReceipt` names a bloom, a previous base, and a new head, and no membership, so a receipt drained
from the outbox after a restart cannot reach the issues it belongs on. The outbox payload on the
landing-receipt topic therefore carries the unchanged receipt together with the landed bloom's ordered
member ids, which the reducer holds at the moment it mints the receipt. The receipt value itself, the land
fact, the land outcome, and the source port are unchanged.

Reading membership back out of the store stays forbidden — it is the self-contained-document rule this same
section states. Recovering it from the pull request's prose stays forbidden too: that would make free-form
platform content an input to projection.

### Failure is skipped, not stalled

Outbox delivery is at-least-once and holds a topic until its entry succeeds, so a permanent condition must
never surface as an error — one unaddressable member would otherwise block the mirror indefinitely. The
adapter classifies instead: an unaddressable id, or a target the repository refuses, is recorded and
skipped, and the entry still settles. Only a transport fault or an unexpected status is an error, and that
is what re-drives. Every write is one request against one target, so a skipped target leaves nothing
half-written.

### The objects already opened

Eight umbrella issues exist under the previous rule — seven for one bloom, one for another — every one of
them a receipt-opened stub, and all of them already closed. No workpiece issue was ever opened, because only
the receipt path ran against this repository. **They stay as they are.** They are a bounded, complete, and
accurate record of what the projection did; deleting issues is irreversible; and closing what is already
closed is a no-op. No reconciliation code ships, and none is needed: the marker scan that made them findable
is the same repository-wide walk this amendment removes, so nothing in the projection path reads them again.

### Consequences

- The projection stops creating objects. Its only creates are comments, its only find is scoped to one
  target's comment list, and repository-wide issue enumeration leaves the projection path — taking with it
  the per-lookup latency that let a poll drain lap a single projection.
- Bloomery's outward surface lands where the work already is: a receipt reaches the issue and the pull
  request a person already has open, and no object appears that nobody will ever close.
- A workpiece whose id is not `issue-<N>` is invisible on GitHub. Accepted — those are the local and fixture
  lanes, whose reader is `GET /view`.
- A wrong repository configuration can address a comment at an unrelated object. Accepted at bounded cost:
  one deletable comment, no authored content lost, and the same configuration fails landing first.
- The projection depends on the landing-branch convention that the source port owns, so the two must read
  that name from one place rather than each spelling it.

### Alternatives considered

- **Keep the shadow issues and make the marker lookup cheap** (an index, a cache, a search query) — removes
  the latency and none of the rest: the objects still have no lifecycle, still never close, still fail the
  title lint, and still duplicate what the repository holds. It optimizes the mechanism of the wrong
  decision.
- **Own a marker-delimited managed section of the source issue's body** — the richest surface, and rejected
  as the one unrecoverable failure mode available here. It requires writing a human's whole body back on
  every update, so any parse, truncation, or concurrency defect destroys authored prose with no undo. The
  content it would carry fits in a comment.
- **Give `WorkpieceId` an issue number, or a variant carrying one** — contradicts §The bloom directly: a
  GitHub issue is a projection of a workpiece, not its identity, and the id outlives any scope revision.
  It would also make every non-GitHub lane construct a number it does not have.
- **Keep an umbrella issue as the bloom's aggregate, and target only that** — one object instead of many,
  and rejected because the object still has no lifecycle of its own and still duplicates `GET /view`. The
  landing pull request is the aggregate that already exists, already carries the diff, and already closes.
- **Carry the landing on the next view reconcile rather than enriching the receipt payload** — the view
  document already retains landed blooms with their membership, so no payload would change. Rejected because
  the bloom view carries no previous-base/new-head pair, so the receipt's actual content would be lost, and
  because it would replace a topic with its own at-least-once delivery by whenever a view reconcile next
  happens to run.

## 2026-08-16 amendment: the projection owns what it creates

ADR-0199 makes a fleet-local store the authority for intent and a fleet-local repository the authority for
source, and settles what is left for the platform: issues become outbound projections only. GitHub stops
being a place where work is decided and becomes a place where it is visible. That decision is what this
amendment records; the boundary's direction, the observation-intake rule, and the prohibition on platform
authentication as an author signature are all unchanged.

The 2026-08-11 amendment narrowed the write surface to marker-keyed comments on objects the repository
already holds, because the objects a projection wrote to were objects people had authored. Under ADR-0199
there is a second class of object: the replica issue a projection creates for a commission that has no
GitHub home and never had one. Nothing is at risk in that object — no human authored it, and its entire
content is rendered from canonical local state.

### What widens

A projection may **create** issues, and may write the title and body of an issue **it created**.

### What does not widen

A projection may **never** write the title or body of an object it did not create. That prohibition is the
load-bearing half of the 2026-08-11 amendment and it survives intact; a person's prose is never round-tripped
through a renderer. Comments on objects the repository already holds keep their existing rule.

The bound holds the same way it held before — by construction rather than by discipline. The create-and-own
verbs live on a separate client contract from the comment verbs, and a projection may address an issue's
title or body only through a number it recorded from its own create. An issue that arrived any other way is
unaddressable by that path, so adopting a human-authored object is not a policy the implementation follows
but a call it cannot make.

Every created object is marked as adapter-owned and carries a visible notice that it is a replica and is not
the place to edit. Edits made there are overwritten on the next projection and are never read as input,
which is the same one-way rule the rest of the boundary already states.

### Why not adopt existing issues instead

Matching a commission to an issue that already exists would avoid duplicate objects during migration, and is
rejected for the reason the 2026-08-11 amendment gives about numbers: nothing can verify that an issue
number was authored against this repository, so an adoption rule is a rule about untrusted input. Creating
the object removes the question — the projection knows the number because it made it. Migration therefore
creates replicas rather than adopting, and a workpiece whose source issue a person wrote keeps that issue as
a human object with comments projected onto it, exactly as today.
