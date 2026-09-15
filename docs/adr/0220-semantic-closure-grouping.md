# ADR-0220: Semantic closure grouping

- **Status:** Proposed
- **Date:** 2026-09-16

## Context

A bloom's members are selected into a shared verification run by
`SealedPolicySelection::select` and planned by `build_plan`
(`crates/aether-chassis-bloomery/src/bloomery/reactor/executor/runtime/scheduler.rs`).
The selector's whole grouping key is: same bloom, covered by the composition
contract (`CoordinationState::composition_contract.covers`), and agreeing on one
construct base (`ConstructContext::starting_head`, ADR-0218 §Amendment). The
coordinator's own blast-radius walk, `affected_closure`
(`crates/aether-bloomery/src/reduce/coordination.rs`), expands over the
seal-declared `depends_on` edges and over composition coverage. Nothing in
either reads the package graph.

Two consequences follow, and both are visible in bloom 0c5a157ecbe4. First,
members that could safely have shared one run did not: five candidates each
verified alone, three of them over the identical two-crate closure
(`aether-chassis-bloomery aether-harness-bloomery`), because their construct
bases differed. Second — the reason grouping is not simply "coalesce more" —
one of those members (issue-6031, a change to `xtask/src/transform/verify/mod.rs`)
verified under a full workspace sweep, so coalescing it with a two-crate sibling
would have made the sibling pay the sweep.

The cost of getting a group wrong is asymmetric, and that asymmetry is what this
decision has to respect. A red shared run does not fail one member: every member
of the run re-runs, so a group of *n* costs *n* lanes when any one of them is
bad. And today's attribution cannot always tell which one was bad. It reads the
paths a failing gate's findings name and maps them onto the members' changed
paths (`attribute_gate` / `MemberExtents::owner`,
`crates/aether-chassis-bloomery/src/bloomery/verify/ownership/`). When member A
changes a shared value type and member B's crate fails to compile against it,
the diagnostic's location is in B's file. B is charged and ejected although B was
correct in isolation; A folds clean and the disagreement is never named.

The worked example is bloom 0c5a's `dispatch-8311-step-0`, and it is worth
reading carefully because it is the *near miss* of that shape rather than an
instance of it. The run went red on `verify.clippy` and `verify.test` with
`error[E0063]: missing fields scope_model_override and scope_seat in initializer
of aether_bloomery::CommissionShowView --> xtask/src/bloom/amend/tests.rs:49`.
Member `retrospect-6a1fe13e6695` added those fields to `CommissionShowView` in
`aether-bloomery`; the initializers that broke live in xtask, which is in that
crate's reverse-dependency closure and is listed in the run's own
`verify.scope.log`. The coordinator ejected the member for it — and that was
correct, because the run carried **one** member and no sibling in the bloom
wrote an xtask path (only `issue-6031` did, in a different run). So this is the
candidate's own miss of a reader in another crate inside its own closure, not a
two-member conflict: the readers table (#6068) is what would have caught it at
construct time, and the annotation this work adds must stay silent on it. Had
the same candidate shared a run with a member that *did* write xtask, path
attribution would have charged that sibling and ejected it — which is the
failure the (b) gate below exists to prevent.

That is what makes closure *disjointness* the load-bearing property rather than
closure *overlap*. Two members whose closures do not intersect cannot break each
other's compilation: neither one's write lands in a crate the other's gates
compile. They are safe to group by construction, whatever attribution can or
cannot do. Members whose closures intersect are the ones a red run cannot sort
out — so grouping them is gated on attribution learning to name the pair.

The closure itself is already computed per candidate. `Scope::of_changed`
(`xtask/src/transform/verify/scope.rs`) resolves it for the verify gate and for
the construct lane's own lint bar, over `reverse_dependency_closure`
(`xtask/src/affected/mod.rs`). The read-side half of this work records that
closure plus the candidate's *written* packages into the construct
`evidence.json` (`PackageClosure::stamp`), serves them from each member's
dispatch row (`api/runtime/evidence/closure.rs`), and shows the derived edges on
the console board. No scheduler behaviour changed; this ADR is the decision
about what the scheduler should do with them.

## Decision

Three changes, in this order. Each stands alone; each later one is gated on the
one before it.

**(a) Group on a shared base by closure-disjointness first, and say so.**
Within the candidates that already agree on bloom, contract coverage, and
construct base, the selector prefers a group whose members' closures are
pairwise disjoint. Disjointness is the safety property: such a group's members
cannot compile each other's writes, so a red run in it attributes by path
without ambiguity and the innocent members are never re-run for a sibling's
defect. A member whose closure is unbounded — the whole-workspace sweep, which
`reverse_dependency_closure` returns as `None` — intersects every other closure
by definition and is therefore never coalesced with anything; it runs alone.
Symmetrically, the selector declines to coalesce a bounded, disjoint pair *onto*
a run whose scope is a full sweep, because the sweep's cost is the group's cost.
Every run records the reason it grouped the way it did — the members' closures,
whether they were disjoint, and which candidate was declined and why — so an
operator reading a one-member run can tell "nothing else was ready" from
"everything else intersected" from "the only peer was a sweep".

**(b) Admit intersecting closures only once attribution understands the edge.**
An intersecting group is admissible when a compile or test finding reported in
member B whose diagnostic names a symbol or path that member A's diff changed is
recorded as an **A↔B semantic conflict** rather than as B's lone defect. Both
members are named; neither is ejected as a defect. The conflict is reconcile
work for whichever of the two would fold second — the first fold stands, the
second member rebases onto it and repairs — which is the same shape ADR-0218
already gives a fold-time collision, moved earlier. Until attribution produces
that verdict, an intersecting pair stays split: the saving is real but it is
bounded by one extra lane, and mis-ejecting a correct member costs a whole
construct lap plus the operator's trust in the ejection.

**(c) Record every bounce with its cause class.** A bounce is a red shared run
followed by a re-run of its survivors. Each one is recorded as `lone defect`
(attribution named exactly one member and no peer's write is in the diagnostic),
`semantic conflict` (the (b) verdict), or `environment` (the existing host-fault
class, `discriminate` / `HostClass`). The study stage reports bounce rate by
cause. Without it, (a) and (b) cannot be evaluated: a grouping change that
halves the lane count while tripling the bounce rate is a loss, and no current
signal distinguishes the two.

**What must be persisted, and where.** (a) needs, at selection time, each
queued request's closure. The selector reads `CoordinationState`, so the value
has to reach the reducer: the natural carrier is `MemberVerifyRequest` — the
request already pins the member, its scope revision, and its candidate, and the
closure is a property of exactly that candidate. It belongs beside
`MemberVerifyRequest::member`, as an `Option<Vec<String>>` whose `None` is the
unbounded answer and never the empty one. That is a wire-format change to the
sealed request type: it changes the request digest, so it lands with a schema
digest bump (`crates/aether-bloomery/tests/golden_decisions/fixtures/schema-digests.txt`)
and every in-flight request is invalidated by it. The read-side half
deliberately does **not** do this — it serves the closure from the evidence the
dispatch row already addresses — precisely so the persisted change is made once,
when (a) is implemented, rather than speculatively now.

(b) and (c) need no new persisted value on the request. The conflict verdict is
an observation, so it rides `LaneObservation` alongside `violating_paths` and
`narrowing`; the bounce cause is a study row.

## Consequences

- A bloom of members touching unrelated subsystems groups and verifies in one
  lane instead of *n*, and the grouping's reason is legible on the board rather
  than inferred from run cardinality.
- A full-sweep member stops silently taxing its siblings: it is excluded by
  rule, and the rule is stated in the run's reason rather than emerging from
  base disagreement by accident.
- Bloom 0c5a's five single-member runs would not all coalesce under (a) alone,
  because three of them also share the `aether-chassis-bloomery` write — they
  are exactly the intersecting case (b) unlocks. The honest near-term win from
  (a) is the disjoint pairs (`retrospect-e32787771dac` against a member touching
  neither `aether-bloomery-github` nor the chassis) plus the removal of the
  sweep from every group.
- Attribution gains a verdict it does not have today, and with it the obligation
  to be right about it: a false semantic-conflict verdict sends two correct
  members to reconcile. The (b) gate is therefore a real gate, not a formality —
  it ships with its own evidence, and the (c) bounce classes are how its error
  rate is read.
- A closure on `MemberVerifyRequest` bumps the sealed schema digest, so (a)
  cannot be landed mid-bloom; it lands with a roll.
- Closures are computed from the candidate's own tree at construct time, so a
  member that is repaired recomputes it. A stale closure would group on a tree
  that no longer exists, which is why the value pins to the candidate rather
  than to the member.

## Alternatives considered

- **Group on closure *intersection* (coalesce members that share crates).** The
  opposite rule, and the intuitive one — shared crates compile once. It
  maximizes exactly the group whose red run cannot be attributed, so it buys
  build reuse with re-runs of innocent members. Rejected: the asymmetry is the
  whole problem.
- **Derive the edges from the seal-declared `depends_on` graph alone.** Already
  available to `affected_closure`, needs no new value. It is a statement of
  intent written before the work, not of what the candidates did; bloom 0c5a's
  members declared no edges at all and three of them wrote the same crate.
- **Compute the closure coordinator-side from the candidate diff at selection
  time.** Avoids the persisted field. It puts a `cargo metadata` + guppy load on
  the reducer's hot path, and the reducer is a pure replayable fold — a value
  derived from the filesystem at fold time is not replayable.
- **Keep the selector as it is and only surface the edges.** This is what the
  read-side half does, and it is worth having on its own. It does not recover
  the lanes: an operator who can see that two members are independent still
  cannot make the selector group them.
- **Attribute by re-running each member alone after a red group (bisect
  always).** Correct and already the fallback (`batch`). It costs one
  `verify.check` per member, which is the cost coalescing was meant to save, so
  it makes an intersecting group break even at best.
