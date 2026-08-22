# ADR-0200: verification is a ledger of proof facts

- **Status:** Proposed
- **Date:** 2026-08-17

## Context

Verification is the Bloomery's dominant cost, and it is spent re-deriving things the system already knows. Every member attempt runs its own verify; the aggregate gate then runs the full workspace suite over the woven tree — and runs it twice when it fails, once to fail and once to discriminate. A land that touches one crate invalidates nothing outside that crate's dependents, yet the next bloom re-proves the whole world as if it might have. The waste grows with the suite: at roughly 4,500 tests, a single aggregate failure costs two full-suite executions to charge one member.

The scarce resource underneath is the build. Model lanes scale sideways — twenty writers are cheap — but the host cannot run twenty rustc processes, so every gate that implies a build serializes the pipeline behind compute that is mostly re-proving unchanged code. Three further leaks compound it: a member charged refine laps for a failure that was already red on its base (three paid laps went to one pre-existing failure in the issue-5020 arc); flaky greens and reds that discrimination catches at the gate but nothing remembers afterward; and refine laps spent asking a model to apply a patch the toolchain had already written (fmt output, `MachineApplicable` clippy suggestions — over half of observed refine findings).

ADR-0196 accepted "tree-addressed proof reuse" as a waste-prevention principle for members inside one bloom, but nothing wires it, and nothing extends it across blooms. ADR-0186 gives the daily branch its linear land order; ADR-0195 classifies failure causes; #5099 makes closure-less recheck discrimination intersect two full-suite runs; #4986 gives lanes thread-bound session resume. Those are the pieces this decision assembles.

## Decision

Verification becomes a **ledger of proof facts** that gates consult and extend, with proof strictness inversely proportional to event frequency: the cheapest gates run at the highest frequency (member attempts), and the only full-barrier gate runs at the lowest (the day roll).

### The fact

A proof fact is a persisted record `(closure_key, test, result, host_class)`. The `closure_key` is a hash over the git subtree hashes of the test's package closure — the set of crate source trees that can influence the test's outcome through the package graph. A fact is content-addressed by what was tested, not by when or where in bloom history it ran: green facts flow forward across blooms, and a land that leaves a closure's subtrees untouched leaves its facts standing.

Two integrity preconditions are non-negotiable:

- Only results that have passed flake discrimination become facts. The ledger is a cache of truth; a flaky green recorded as fact later attributes an innocent member with certainty. The discrimination machinery (#5099) guards the ledger, not just the gate that ran the test.
- Facts key on host class. A green on the fleet host is not a green on a GPU host; host-conditional tests are real (#5021) and a fact must not travel across host classes it was never proved on.

### The gate ladder

- **Member verify** proves the closure of the member's own diff. This is current behavior — `verify.test` already resolves breadth from the diff base — and is unchanged.
- **The weave gate** proves the *intersection* of member closures. Each member already proved new-self against old-siblings; the weave is the first tree where changed closures meet, so only tests downstream of two or more members' diffs carry new information. Disjoint members produce an empty intersection, and an empty gate means the land is free. The aggregate gate today runs the full suite because no closure is threaded into it; this is the single largest compute cut in the decision.
- **Landing is immediate** on a green weave gate. Land latency stops scaling with suite size; what protects the mainline is the ledger's coverage discipline below, not a wait.
- **Daily sweeps** run lazily on idle prover time, scheduled by the coordinator, converting the day's unknown facts to green or red. A red taints its scope: blooms touching the tainted closure hold, attribution consults the taint, and a repair workpiece is filed automatically. The culprit is found by bisecting the day's linear land order re-running the one failing test — O(log lands) executions of a single test, not re-runs of the suite.
- **The roll is the one full barrier.** Main receives the day's tree only when the coverage map is fully green. Everything cheaper upstream is justified by this backstop.

### Attribution through the ledger

A failure at any gate resolves against the ledger three ways:

- A green fact exists at the base's closure key: the member's diff broke it. Certain, no rerun.
- No fact exists: probe the base with that one test, then record what the probe proved.
- The base probes red: the failure predates the member. A base-repair workpiece is filed and the member is not charged a refine lap. This retires the issue-5020 class outright.

### The batch gate

Disjoint-surface members compose eagerly, and one build gate over the composition proves everyone: the batch run is a fact producer, emitting facts for every member's closure key from a single build. "Member done" means its facts are green — batch membership is an execution-level detail, never a semantic one, so a member neither waits for nor answers for its batchmates beyond sharing the build.

Accumulation is adaptive: verification starts when work exists, and a young build restarts when substantially more work arrives — eight members finish and the gate starts; twenty-four more finish moments later and the gate restarts over thirty-two rather than running twice. The restart threshold preempts only young builds or large additions.

Failures feed backward: an error attributed to a member returns to the lane session that authored the code, resumed in place (#4986), so the fix is written by the context that wrote the bug. Attribution inside a batch follows a ladder — a file-owned error maps straight to its member (disjoint surfaces make this a lookup); a closure-owned failure resolves through the ledger attribution above; the unowned residue (feature unification, trait coherence, wire-tail collisions) bisects the batch, the same O(log n) rung as the sweep's culprit finder.

### Fix-forward

Mechanical findings never spend a model lap:

- fmt output is applied, not reported.
- `MachineApplicable` compiler and clippy patches are applied mechanically and re-verified; only the residue falls through to a model lap.
- Reviewer-prescribed patches are explicitly out of this decision. A model asserting a fix is correct is not a tool asserting it, and multi-author candidates change the review contract; that question waits for its own record.

### Soundness boundary

The package graph is the soundness boundary for closure-scoped proof, and the known leaks are priced in rather than wished away: per-package feature unification can differ from workspace unification (the low-frequency full gates — sweeps and the roll — exist precisely to catch what closure scoping cannot); runtime resource contention produces flakes, which wider gates make worse, not better, and which discrimination plus the fact-integrity rule handle; wire-frozen tail-append collisions and inventory registration are visible in the package graph, so the intersection gate catches them.

### Base admission

The base tree is a subject of verification in its own right. A member's work order is withheld until its base tree holds a green receipt under the whole-workspace gate set (`VerifyGateSet::base()`, distinct from a member's closure-narrowed set). An unproven base queues one `verify.base` dispatch rather than refusing the seal; a red base is a day-level stop (`InterruptKind::BaseRed`), not a member's charge. The receipt is the diagnostic set a later slice subtracts at Member-Verify.

## Consequences

- Verification compute becomes proportional to new information rather than to suite size. Disjoint work lands for free; the full suite runs on idle time and at the roll, not on the land path.
- Attribution stops charging innocents: pre-existing reds become base-repair workpieces with no refine lap spent, and certain attribution needs no rerun at all.
- The refine-lap class dominated by toolchain-authored patches disappears into fix-forward; model laps are reserved for findings that need judgment.
- New machinery is owed: a fact table in the journal, closure-key computation over the package graph, closure threading into the aggregate gate, the sweep scheduler with taint and auto-filed repair workpieces, the batch composer with its restart threshold, and the bisect rung. Each slice lands independently and pays for itself; the sequence runs from threading closures into the aggregate gate (no ledger needed, immediate cut) through fact recording, consultation, batching, sweeps, and finally the roll barrier.
- The ledger changes what ADR-0198 leases are for: batching dissolves the shared-prover load case (one large build takes the whole machine and that is correct), and leases retain value for genuinely exclusive resources — the mainline lock, a GPU host, disk. ADR-0198's implementation remains held as recorded.
- Immediate land composes with ADR-0199's ownership direction; the mainline-protection configuration that permits it is operator-owned, not machinery.
- A red discovered by a sweep is discovered after its land. This is accepted deliberately: the day branch absorbs it, taint plus auto-repair bounds it, and the roll barrier keeps it from ever reaching main.
- The day gains a verification position belonging to no bloom, and the seal gains a precondition about the fleet rather than the draft.

## Alternatives considered

- **Keep the full-suite aggregate gate** — cost grows linearly with the suite forever, and at current scale already dominates wall-clock; rejected as the thing being fixed.
- **Scale out with more prover hosts** — buys throughput proportional to money without removing re-proof waste, and host-class facts would still be needed the moment hosts differ; orthogonal at best.
- **Per-member serial gates without composition** — N members cost N builds; the batch gate produces the same facts from one.
- **Artifact caching alone (sccache status quo)** — caches compilation, not proof: tests re-execute and failures still attribute by rerun; necessary but not sufficient.
- **Trust runner-reported results as facts without discrimination** — poisons the ledger with flakes and converts them into certain false attribution; rejected on the integrity precondition.
- **A memberless bloom running the base verify (ADR-0205's unit)** — rejected; ADR-0205 is unimplemented and self-blocking under the one-active-bloom rule. A base-verify bloom would be a sealed unlanded bloom, so it would block the very member seal it exists to unblock.
