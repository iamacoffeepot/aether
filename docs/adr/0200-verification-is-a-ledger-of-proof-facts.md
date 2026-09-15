# ADR-0200: verification is a ledger of proof facts

- **Status:** Accepted
- **Date:** 2026-08-17
- **Amended:** 2026-09-09 — status: implemented on `main`; proof facts are written on one path, `crates/aether-chassis-bloomery/src/bloomery/verify/facts.rs`, keyed by `(closure_key, test, host_class)` behind flake discrimination (#5142).
- **Amended:** 2026-09-15 — gates invalidate by delta class; a re-verify runs the gates its delta can reach and carries the rest from the earlier receipt (see [Amendment: gates invalidate by delta class (2026-09-15)](#amendment-gates-invalidate-by-delta-class-2026-09-15)).
- **Amended:** 2026-09-15 — a single passing contextual run records one green fact per declared gate (#5948, #5986; see [Amendment: a single green contextual run is a fact (2026-09-15, #5986)](#amendment-a-single-green-contextual-run-is-a-fact-2026-09-15-5986)).

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

**Amended by [ADR-0218](0218-contextual-verification-and-eager-integration.md):**
the original batch paragraph below describes the earlier direction, not the
proof contract of the contextual implementation. A composed pass belongs to
the exact node and complete execution contract; it never creates standalone
parent facts. The existing ledger accepts only independently discriminated
observations under that contextual input address. Running plans are immutable
and new arrivals join the next run; no young-build restart policy applies.
Attribution is set-valued and evidence-backed: path ownership is a suspect,
both failing halves are investigated, interactions remain group-owned, and
unreached or missing probe results remain unknown. Physical cost is charged
once to the bloom. The gate ladder, sweep, and day-roll directions elsewhere
in this ADR are not authority to bypass the current atomic Resolve or final
aggregate gate.

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

## Amendment: gates invalidate by delta class (2026-09-15)

Proof reuse above is all-or-nothing on the whole tree: a green proof over tree `T` under gate set `G` answers a later verify of `T` under `G`, and answers nothing at all about `T'`. A repair lap changes the tree by definition, so every delta-confirm after a Refine or a Reconcile falls off the reuse and buys the whole umbrella again.

Two members of bloom `0f16e207` measure the cost. `issue-6023`'s step 0 was red only on `verify.suppress` — two missing `// aether-suppression-request:` marker lines. Its refine lap changed those two comment lines; the re-verify (run `93D384A0` and after) ran clippy, docs and test again in full, five to ten minutes each. `issue-6024` was red only on `verify.docs` — one private intra-doc link in a `///` line — and its re-verify rebuilt and retested everything the same way.

The finer fact is per gate. A gate reads some of the tree; a delta touches some of the tree; where the two do not meet, the gate's verdict over the new tree is the verdict it already gave over the old one. So a re-verify **classifies its delta** against the tree the earlier receipt proved, runs the gates that class can affect, and **carries** the rest.

### The table

One class per changed line of a Rust file, one per path for everything else; a delta's classes are unioned, and the gates it invalidates are the union of their rows.

| Delta class | What it is | Invalidates |
| --- | --- | --- |
| `code` | Rust tokens changed — or the hunk could not be classified | every gate |
| `doc_comment` | only `///` / `//!` lines | `docs`, `fmt`, `dup`, `suppress` |
| `comment` | only ordinary `//` lines, blank lines, or suppression attributes (`#[allow(…)]`, `#[expect(…)]`, `#[ignore]`) | `fmt`, `clippy`, `dup`, `suppress` |
| `manifest` | a `Cargo.toml` | `deps`, `lock`, `clippy`, `docs`, `test`, `suppress`, `fmt` |
| `lockfile` | `Cargo.lock` | `lock`, `deps` |
| `inert` | `docs/**` and the top-level `*.md` | nothing |

`verify.preflight` and `verify.containment` are not in any row and are never carried: neither is a gate the fan-out runs over the tree. Preflight is the umbrella's refusal to start, and containment reads *which* paths moved rather than what changed inside them, so no class of line edit excuses it.

Three rows read wider than the first statement of this table did, because the scanners do. `manifest` invalidates `suppress` (the suppression scanner dispatches on file name and scans `Cargo.toml` as well as `.rs` and `.jscpd.json`) and `fmt` (`cargo fmt --all` resolves workspace membership out of the manifests). And `inert` is a narrow allowlist rather than "any non-Rust file", because the non-Rust files a repair lap is most likely to reach for are the ones that decide gate verdicts: `rustfmt.toml`, `clippy.toml`, `rust-toolchain.toml`, `scripts/check-suppressions.py`, a test fixture, and the `xtask/src/transform/*.md` files a crate `include_str!`s. All of those are `code`. Where a scanner reads something the table does not name, the table is wrong — never the gate.

The table is one value in one place, `crates/aether-bloomery/src/values/verify_delta.rs`, read by both sides of the ledger. The lane classifies its own diff (`xtask/src/transform/verify/delta.rs`) and selects from the table; the coordinator's admission door judges the resulting coverage claim against the same table. Two tables would be two answers to one question, and the looser one would decide.

### Carrying is not passing

A carried gate is neither run nor passed. `evidence.json` records each one by name — the gate, the receipt digest it carries, the tree that receipt proved, and the delta classes the claim rests on — beside the `gates` timings of what ran and the `failed_verifiers` of what failed, so a reader can tell the three apart.

The carry needs both halves and neither side can invent the other's: the lane knows what changed, the ledger knows what was green. The host states the proved tree, the receipt, and that receipt's green gates to the lane; the lane intersects them with what its delta cannot reach. A gate the receipt failed is never carried — that is the gate the re-verify exists for.

### Admission

A receipt whose coverage says "ran these, carried those from receipt `R` over tree `T`" is admitted only when `R` is on record for exactly `T` and every carried gate is one the stated classes cannot affect per the table. Otherwise the receipt is refused as incomplete and the full umbrella is dispatched. The lane errs the same way: an unreadable diff, an unparsed hunk, a binary file, or a path outside the inert allowlist is `code`, which invalidates everything and reduces the run to the umbrella that existed before this amendment.

This changes nothing about a member's first verify — there is no prior receipt, so everything runs — and nothing about the contextual step 0 of a shared run.

## Amendment: a single green contextual run is a fact (2026-09-15, #5986)

Commit 7fa229469 (#5948) admits one exception to the integrity rule above —
only flake-discriminated results become facts — and to the rejected alternative
below that trusts single runner results. `record_green_contextual_facts`
(`crates/aether-chassis-bloomery/src/bloomery/verify/contextual_facts.rs`)
records one green fact per declared gate from a single passing contextual run,
through `DiscriminatedFacts::from_green_run`, and `reuse_contextual_proof` lets
a later shared run over the same inputs skip physical work on that observation.
The exception exists because waiting for a second suite meant production
recorded nothing at all.

The bound is the address. A contextual fact is keyed by `contextual_fact_key` —
the exact candidate tree and checkout, the ordered member coverage, and the
complete composition contract — for the gates the sealed contract declares, on
the contract's host class, and it answers only those inputs: any mismatch of
node, contract, host class, or gate set is a cache miss, and the latest row per
gate must be green or reuse refuses. A single observation therefore cannot
charge a different tree, member, or contract; the most a flaky green can do is
skip re-execution of the identical inputs it already passed once.

What bounds that residue is re-execution everywhere else. A later red over
overlapping coverage still runs and still attributes through probes that
execute the check — no recorded green excuses a red run — and the two-run
`record_contextual_facts` path still stands beside the single-run one, so a
later red observation at the same address supersedes the green and disables
reuse. The sweep and the roll keep their own discriminated proof at their own
addresses rather than revisiting recorded rows: the sweep converts only unknown
addresses with two-run discrimination, and the roll holds the day until its
coverage is green. Every reuse is journaled as `Fact::ProofReused`, naming the
gate, the contextual closure key, and the producing dispatch, so a run that did
no physical work reads as one.

Marking single-run facts provisional so they cannot be reused was considered
and rejected: it re-creates the production state #5948 was landed to end,
where nothing was ever recorded. ADR-0218's contextual-proof section points
here.

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
