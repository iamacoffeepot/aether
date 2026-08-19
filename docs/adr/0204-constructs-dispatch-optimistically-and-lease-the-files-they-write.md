# ADR-0204: constructs dispatch optimistically and lease the files they write

- **Status:** Proposed
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
   tree; the first observed write to a path acquires an exclusive per-file
   lease for that member, held until the member integrates or is retired. A
   write to an undeclared path inside the surface acquires its lease
   automatically — that is the fault path the ripple data requires. A write
   outside the surface remains a containment-gate verify failure. Leases are
   visible in the operator projection with holder and age, per ADR-0198.
   Observation cadence is implementation latitude; capture of a candidate is
   the latest permissible observation point.
4. **Contention resolves by canonical id, which is total, so no deadlock is
   possible.** When an earlier-id member writes a path a later-id member
   holds, the later holder is evicted with resume: its session state
   persists, and it re-dispatches on the advanced base after the earlier
   member integrates. When a later-id member writes a path an earlier-id
   member holds, the later member continues its other work and takes the
   rebase at integration.
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
- True contention — bounded above by the measured 39.6%, expected lower now
  that the containment gate removes the out-of-surface class — costs an
  eviction-with-resume or an integration rebase instead of a fold wedge, and
  the losing lane stops at first touch rather than after finishing doomed
  work.
- The lease table is small and cheap: entries on the order of the sum of
  live write sets (median 7 files per member across a single-digit member
  count), one hash operation per observed write, and a working-tree scan per
  lane per observation tick.
- The executor gains observation, eviction, and resume machinery; the
  operator projection gains a lease surface; scope authoring gains two
  optional declaration lists. Read declarations stay load-bearing-only by
  policy — exhaustive read declarations at glob granularity would rebuild
  exactly the false serialization this decision removes, as the 13%
  utilization figure shows.
- Follow-on work: demoting surface-derived edges in the reducer; the lease
  table, observation, and preemption in the executor; the declaration
  vocabulary in commission scopes and the admission door; the lease view in
  the operator projection. ADR-0198 remains the general lease decision —
  this applies it to workspace files. ADR-0202's composition vocabulary
  gains the read/write declaration terms.

## Alternatives considered

- **Keep glob-derived dispatch gating** — measured: 60% of the pairs it
  serializes never conflict.
- **Optimistic dispatch with redo only, no leases** — loses early
  stop-at-first-touch; at the measured collision rate, doomed constructs
  routinely run to completion before the conflict is discovered.
- **Lease reads as well as writes** — protects against a torn read the
  per-lane snapshot already prevents, at the price of re-serializing every
  wave through the files all lanes read.
- **Complete upfront file declaration as a contract** — falsified by the
  ripple measurement; half the out-of-core changes are not nameable in
  advance.
- **Narrower authoring globs instead of leases** — moves the prediction
  burden onto the author; the wave-15 wedge is the record of how
  under-declaration fails.
