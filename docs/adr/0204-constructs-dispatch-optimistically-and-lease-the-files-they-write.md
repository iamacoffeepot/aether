# ADR-0204: constructs dispatch optimistically and lease the files they write

- **Status:** Accepted (amended 2026-08-21 — §4's eviction is retracted: a shared file is a merge at integration, not a stopped lane. See [Amendment](#amendment-2026-08-21-a-shared-file-is-a-merge-not-an-eviction).)
- **Date:** 2026-08-19

## Context

Bloom members declare surface globs, and the coordinator derives an edge
between any two co-sealed members whose globs overlap (ADR-0196). A derived
edge gates dispatch: the later member's construct does not start until the
earlier member integrates. The glob is an authority boundary, so authors
declare it wide — the containment gate rejects out-of-surface edits, and the
wave-15 wedge showed what an under-declared surface costs. Wide surfaces and
overlap-derived dispatch gating together produce serialization the work does
not require.

Two measurements over the coordinator journal and fleet history
(2026-08-19, operator-verified by independent re-derivation) size the
problem:

- Of 134 landed member pairs whose declared surfaces overlapped, 53 (39.6%)
  actually modified at least one common file. The other 81 (60.4%) paid
  dispatch serialization for a conflict that never existed. Counterfactual
  lane counts on two historical blooms: one opened 1 construct lane where 5
  members were dispatchable, another 4 of 7. None of the collisions were
  `Cargo.lock`-only. The 39.6% is an upper bound on the honest rate: seven
  pairs collided through edits *outside* a declared surface, a class the
  containment gate now converts into verify refusals.

  *Amended 2026-08-21:* this figure counts pairs that touched a common
  **path**. It is not the rate at which two members' edits actually conflict,
  and the amendment below shows it over-counts that rate badly — a pair
  editing disjoint hunks of one file three-way merges cleanly and appears here
  anyway. Read it as the rate at which a pair's trees have to be *merged*, not
  the rate at which merging them fails.
- Across 162 landed members, the median write set is 7 files (quartiles
  3–13, maximum 83), and a median 78% of a member's changed files sit in its
  single densest directory. Of the 678 changed files that fell outside those
  core directories, roughly half are mechanically predictable classes
  (integration tests, module registrations, manifests) and 44% are ordinary
  source files no author would have named in advance. Members touch a median
  13% of the path-space their declared globs admit.

Three prior decisions frame the fix. ADR-0198 decides that contended
resources are leased with visible return times. ADR-0189 gives fold
conflicts a reconcile stage. And every lane already works in its own
checkout of the sealed base, so a lane's reads are snapshot reads — a lane
never observes a sibling's half-written file, only an aging snapshot.

The write-set data forecloses one tempting design directly: a complete
upfront file declaration cannot be a contract, because half the ripple is
unpredictable even to a careful author. And the snapshot forecloses another:
leasing reads (one writer, no readers) would re-serialize every wave through
the hot files all lanes read for context, to protect against a torn read
that the per-lane checkout already makes impossible.

## Decision

Exclusivity moves from glob-level dispatch gating to file-level leases,
acquired at first observed write.

1. **Edges split by provenance.** A declared edge — authored because one
   member builds on another's output — keeps gating dispatch. A
   surface-derived edge no longer gates dispatch; it survives only as
   integration order (canonical member-id order, unchanged). Every member
   without a declared dependency dispatches at seal.
2. **Declarations gain read/write distinction.** Inside its surface, a
   member may declare the files it expects to write and the interfaces it
   load-bearingly reads. The write declaration pre-seeds the lease table; it
   is an optimization, never a contract. The read declaration derives a
   conditional ordering: it binds only against a co-member that actually
   writes into the declared interface — evaluated when such a lease appears,
   as an edge if the reader has not yet dispatched, as a rebase at
   integration if it has. Reads are never leased and never blocked.
3. **Write leases.** The executor observes each construct lane's working
   tree; the first observed write to a path takes a per-file lease for that
   member, held until the member integrates or is retired. A write to an
   undeclared path inside the surface takes its lease automatically — that is
   the fault path the ripple data requires. A write outside the surface
   remains a containment-gate verify failure. Leases are visible in the
   operator projection with holder and age, per ADR-0198. Observation cadence
   is implementation latitude; capture of a candidate is the latest
   permissible observation point.
4. **Contention is a merge, not a race** (amended 2026-08-21, #5401). Two
   members writing one file are two edits that have to be combined, and
   whether they combine is a property of the *hunks*, not of the path — which
   is all a lease can see. So no lane is stopped, in either canonical
   direction. Every construct lane runs to completion, and the integration
   fold merges each member's candidate onto the accumulated tree in canonical
   member order, which is total. A clean three-way merge costs nothing: no
   cancel, no re-dispatch, and no machinery roll. Only a merge that reports a
   textual conflict costs anything, and it costs what ADR-0189 already prices
   — the later-canonical member takes a reconcile lap on the advanced base,
   on the session its lane never left, seeded from the candidate that lane
   produced. The lease table survives as the observation §3 renders and as the
   signal that says which pairs will meet at the fold.
5. **The staleness backstop is unchanged.** A semantic interaction that
   involves no common file — a renamed function, a moved invariant — is
   caught by aggregate verification at fold and repaired by the existing
   reconcile/rebase lap (ADR-0189). Leases narrow the conflict window; they
   do not replace verification.

## Consequences

- The measured 60% of overlap-serialized pairs stop waiting: blooms open at
  the width of their declared dependencies, not the width of their glob
  unions. On the two counterfactual blooms this is 5 lanes instead of 1 and
  7 instead of 4 at dispatch.
- A shared file costs an integration merge and nothing more. Where the merge
  is clean — which the path-level 39.6% over-counts, since it counts every
  pair that has to be merged rather than every pair whose merge fails — the
  pair costs exactly what two unrelated members cost. Where it conflicts
  textually, it costs one ADR-0189 reconcile lap on work that already exists,
  rather than a cancel that discards work which would have merged.
- The lease table is small and cheap: entries on the order of the sum of
  live write sets (median 7 files per member across a single-digit member
  count), one hash operation per observed write, and a working-tree scan per
  lane per observation tick.
- The executor gains observation and a lease surface in the operator
  projection; scope authoring gains two optional declaration lists. Read
  declarations stay load-bearing-only by policy — exhaustive read
  declarations at glob granularity would rebuild exactly the false
  serialization this decision removes, as the 13% utilization figure shows.
- Follow-on work: demoting surface-derived edges in the reducer; the lease
  table and observation in the executor; the declaration vocabulary in
  commission scopes and the admission door; the lease view in the operator
  projection. ADR-0198 remains the general lease decision — this applies it
  to workspace files as a visibility surface. ADR-0202's composition
  vocabulary gains the read/write declaration terms.

## Amendment (2026-08-21): a shared file is a merge, not an eviction

The decision as first written made the lease exclusive: an earlier-canonical
member writing a path a later one held evicted the later holder, cancelling
its lane, and the later member re-dispatched on the advanced base once the
earlier one integrated.

Bloom `4360e7e4a081` falsified the premise. `issue-5379`'s construct lane was
evicted by `issue-5376` on one file: 5376 changed `close_issue` and added
`upsert_comment`, with hunks at roughly lines 11, 200, 250, 400–460 and 760;
5379 inserted three functions between lines 114 and 127. A three-way merge
applies cleanly. The eviction still cost about five minutes of finished lane
time and a machinery roll off a budget of three, for a conflict that did not
exist and could not have — a lease sees the path, and the path is not where
the collision lives.

So §4 is retracted and replaced above. `LaneWritesObserved` stays exactly
where it was, and so does the table it folds: it is the operator's answer to
"who is writing this file" (ADR-0198) and the honest predictor of which pairs
the fold will have to merge. What it stopped doing is stopping lanes. The
mechanism that resolves a real collision was already built and already
proven — ADR-0189's reconcile stage — and it has the property the eviction
lacked: it fires on the trees, after the work exists, so it can tell a
conflict from a coincidence.

One compatibility note. `Outcome::LeasesObserved.evicted` is journaled at a
fixed position (ADR-0187), and rows written before this amendment carry
holders in it. The field is kept and always empty, the snapshot still folds a
recorded eviction, and the integration resume still redeems one — otherwise a
coordinator upgrading onto such a journal would strand the member the old
binary stopped.

## Alternatives considered

- **Keep glob-derived dispatch gating** — measured: 60% of the pairs it
  serializes never conflict.
- **Stop the later lane at its first shared write** (the original §4) —
  retracted above: it prices a shared path as a conflict, and the measured
  case shows the two are not the same thing.
- **Optimistic dispatch with redo only, no leases** — the amended decision is
  nearly this, and keeps the table anyway: the operator surface ADR-0198 asks
  for is worth one hash per observed write even when nothing acts on it. The
  original objection — that this loses stop-at-first-touch — assumed the
  path-level rate was the conflict rate.
- **Lease reads as well as writes** — protects against a torn read the
  per-lane snapshot already prevents, at the price of re-serializing every
  wave through the files all lanes read.
- **Complete upfront file declaration as a contract** — falsified by the
  ripple measurement; half the out-of-core changes are not nameable in
  advance.
- **Narrower authoring globs instead of leases** — moves the prediction
  burden onto the author; the wave-15 wedge is the record of how
  under-declaration fails.
