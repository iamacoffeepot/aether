# ADR-0185: Bloom Train — Successors Seal on a Projected Post-Land Head

- **Status:** Proposed
- **Date:** 2026-08-13

## Context

The admission ref realizes "one sealed, unlanded bloom per mainline" (ADR-0150 §The claim registry): bloom N+1 cannot seal until bloom N's landing proposal merges and its land receipt is admitted. Landing is therefore a pipeline barrier — every seal-to-land cycle is dead time for the next bloom's construct lanes, however much lane capacity sits idle. Width (more members per bloom) amortizes the aggregate stages but couples unrelated members into one landing unit and one squash commit; a queue of small focused blooms preserves revert granularity, and today the invariant forbids the queue. This is the problem merge trains solved for CI: let a successor build optimistically against a projected result and resync when the projection misses.

Every primitive a train needs already exists and has been exercised on live blooms: supersession with claim transfer at identical scope revision, the observed-base rebase arm (#4709), mainline advance held under an active bloom, and proof identity that is already tree-shaped — a checkpoint's identity is its tree, and the correspondence store resolves commits to trees everywhere the source port touches git.

One fact shapes the whole design: the post-land **commit** is unknowable before the merge (squash mints a new sha), but the post-land **tree** is not — when nothing else touches mainline in between, the squash commit's tree is exactly the proposal branch tip's tree, which exists the moment the predecessor resolves.

## Decision

A successor bloom may seal while its predecessor is still in flight, on the predecessor's **projected post-land head**: a base whose identity is the predecessor's resolved tree. The train is bounded at **depth one** — one bloom landing, one bloom building. On the predecessor's land receipt, exactly two things can happen:

- **Projection held.** The landed commit's tree equals the projected tree. The successor's base is bound to the concrete landed commit through the correspondence store — a rename, not a rebase. No worktree content changed, so every proof the successor's members have accumulated remains valid; its lanes never stopped.
- **Projection missed.** The landed tree differs (an out-of-band merge advanced mainline mid-train, the predecessor was superseded or closed, or its proposal changed after the successor sealed). The successor takes the existing supersession rebase arm onto the observed head, carrying claims and configs exactly as a manual supersede does today. Proof is head-bound and stays head-bound: a resync re-proves; there is no train exception.

Landing order is strict: the successor's proposal may not merge before the predecessor's land receipt is admitted — `poll_land` gates on the predecessor's terminal state, so out-of-order merges are refused rather than reconciled after the fact.

The admission ref generalizes from "one unlanded bloom" to "one train": its claim commit carries one `Bloom-Id` line per train slot in landing order. Depth one means at most two lines; the sweep and reconcile paths read the same lines they read today.

## Consequences

- Landing stops being a barrier: construct and verify capacity becomes the throughput limit, and the landing cadence approaches the aggregate-plus-CI tail of each bloom rather than the whole cycle.
- The miss path is not an edge case to fear but the ordinary consequence of humans pushing to mainline mid-train — it degrades to exactly today's supersession ceremony, which is proven machinery.
- A resync discards the successor's in-flight member proofs (head-bound proof, no exception). Salvaging candidates across a resync by re-verifying them on the new base is a follow-on optimization, deliberately out of scope here.
- Depth one captures most of the win: a construct lap and a landing tail are of the same order, so a deeper train would mostly stack resync liability. Deepening is a future ADR if measurement says otherwise.
- The reducer gains vocabulary: the projected-base value, the bind-on-receipt transition, and the train slot in the admission claim. The journal records the projection and its outcome, so the calibration ledger can count how often projections hold — the number that would justify (or refute) a deeper train.

## Alternatives considered

- **Wider blooms only.** Rejected: width couples failure (the slowest member gates the landing) and collapses history granularity (one squash per bloom). Width and the train compose; neither replaces the other.
- **N-deep trains.** Deferred: a miss at slot 1 cascades resyncs through every later slot, and depth one already keeps lanes saturated at current lap times.
- **Projecting the post-land commit instead of the tree.** Rejected: the squash sha does not exist before the merge, so a commit projection could only ever miss; the tree is the identity that survives the squash, and it is the identity bloomery's proofs already use.
- **Constructing the landed commit ourselves (fast-forward landing).** Rejected: it changes the landing trust model — the proposal merge is the owner-visible act that lands a bloom, and this ADR deliberately leaves that act untouched.
