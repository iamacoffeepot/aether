# ADR-0221: Every candidate is judged before it folds

- **Status:** Proposed
- **Date:** 2026-09-15
- **Supersedes:** ADR-0153 §The member line ends at Verify (the rest of ADR-0153 stands)

## Context

ADR-0153 ended the dispatched member line at `Verify`
(`StageCatalog::MEMBER_LINE`, `StageCatalog::next_member_stage`) and moved the
model review to one `AggregateReview` over the integrated product. Its argument
was cost and position: per-member review taxed decomposition exactly where the
machine wanted to decompose, reviewer variance made a re-roll a re-roll rather
than a confirmation, and only the fold could show cross-member defects.

Two things have changed since.

The construct seat is no longer the seat that argument was priced against.
ADR-0146's calibration put Construct on a frontier model; `StageCatalog::profile_of`
now puts it on `MUSE_MODEL` — the contributor tier — because that is what makes
running every model lane affordable. The owner's instruction is verbatim: *"We
just need to get the Muse producer up to a quality that is acceptable."* Under
ADR-0153 the only thing standing between that producer and the product is
`verify.check`: a compiler, which has no opinion about whether the change is the
change the commission asked for.

And the aggregate position cannot supply that opinion per member. It judges a
union diff after the fold, so one member's defect is one hunk among many, and
its remedy — ADR-0153's findings freeze, decompose, route — pays a Refine lap
plus a re-verify plus a delta-confirm to undo work that was already in the
product. In bloom `0c5a157ecbe4` on 2026-09-15 no `Review` dispatch happened at
all: the only judgement in the whole bloom was one `AggregateReview` over the
folded product.

The position was never removed from the vocabulary, only unbound. `StageId::Review`
exists, `reduce::admin`'s `MEMBER_RERUNNABLE` lists it, `ModelOverride`'s
per-stage bindings name it, all five sealed seat bundles in
`xtask/src/bloom/profiles.toml` key it, `Transformation::for_member_stage`
already builds its `review.critic` lane, and `DispatchPriority` already
classifies it `Judge`. What was missing was the line.

ADR-0218's amendments are the other force. §Amendment: eager integration
assembles the product says a green member folds into the product immediately,
and §Amendment: low tolerance says a member that does not go green leaves. Both
make "green" the decisive word, and under ADR-0153 green meant *compiled*.

## Decision

### The member line is `Construct → Verify → Review`

`StageCatalog::MEMBER_LINE` gains `StageId::Review` as its terminus, so
`next_member_stage(Verify)` is `Some(Review)` and `next_member_stage(Review)` is
`None`. A passing `Verify` advances the member rather than resolving it; a
passing `Review` does what a passing `Verify` did — it mints the member's
`ResolutionClaim`, binding the exact candidate tree `reduce_integrate`
re-checks, and the member integrates.

`Review`'s dispatch is the `review.critic` lane at member scope: one judge over
one member's own candidate, against the commission that ordered it and the range
`base..checkout`, where `base` is the member's own construct base — the same
range its `Verify` narrowed against (ADR-0218 §Amendment: a member is verified
over the context it was built on). It names that range as
`Transformation::diff_base` for the reason `for_aggregate_review` does: the
candidate is committed by the time the judge runs, so a lane left to read the
working tree judges an empty candidate (#4723). Its seat and wall clock come
from the sealed catalog's `Review` binding and whatever `ModelOverride` the
bloom sealed over it — `muse-build-sonnet-judge` binds `Review` to
`claude-sonnet-5`, which is the cross-seat independence this decision exists to
buy.

The binding consumes `bloom.verify_evidence` rather than `bloom.candidate`, and
the dispatch carries the passing verify's receipt digest as a second
`Transformation::input`. The judge is shown what the compiler already said
rather than re-deriving it by eye, and the journal records which verdict the
judgement stood on.

`Refine` and `Reconcile` still return to `Verify`. A repair lap changes the code,
so the compiler answers again before the judge does, and the line carries on to
`Review` from there.

### A red Review is routed by the disposition a red Verify is routed by

`Fact::ReviewFailed { bloom, workpiece, evidence, findings }` carries the
verdict — its own fact rather than a failing `AttemptCompleted` because both arms
below need the judge's prose, and an `AttemptCompleted` has no channel for it.
The bloom's sealed `CoordinationPolicy::red_verify` decides:

- **`Eject`** (the default) withdraws the member exactly as a red `Verify` does
  under ADR-0218 §Amendment: low tolerance — the lane is cancelled, the claim ref
  and membership are released, the member is skipped by every completeness fold,
  and **the candidate stays on its ref**. The findings are composed into the
  withdrawal reason by the same `ejection_reason` that composes a verify
  ejection's, so the two read alike to whoever picks the candidate up.
  `WithdrawalCause::Verify` covers it: its own reasoning is that one cause serves
  every verdict answering "this member's work did not pass and nobody is
  repairing it inside this bloom", and which gate refused is the reason's first
  clause.
- **`Refine`** spends one repair roll and re-enters the repair lane with the
  findings as its work order — the same `review_findings` row the aggregate
  path's repair lap reads, written by intake at the member's own Review. The lap
  returns through `Verify` and on to `Review`. The ceiling is the sealed
  catalog's `Review` retry budget: a lap that hands back a tree the judge already
  found against would otherwise re-enter forever, and under `Eject` the question
  never arises because the first red verdict is the last one.

### AggregateReview is unchanged

It stays the product-level judge over the folded tree, with ADR-0153's
freeze/route/confirm loop and its two-pass ceiling intact. The two positions
answer different questions — this one asks whether a member did what it was
commissioned to do, that one asks whether the assembled product holds together —
and cross-member integration defects remain visible only at the fold.

## Consequences

- Review cost returns to being linear in the member count. That is the tax
  ADR-0153 removed, and it is being paid back deliberately: it buys a judge
  between a contributor-tier producer and the product, which is the quality gate
  the current seat calibration has none of.
- A defective candidate is kept *out* of the product rather than routed back out
  of it. Under `Eject` the cost of a bad member is one wasted construct plus one
  verify; under ADR-0153 it was a fold, an aggregate review, a decomposition, a
  Refine, a re-verify and a delta-confirm.
- Reviewer variance now applies per member. A member may leave on a finding a
  second roll would not have produced — the same trade ADR-0218 §Amendment: low
  tolerance already priced for the mechanical gate, and answered the same way:
  the candidate stays on its ref and the member is re-scoped into a later bloom.
- File leases and the evictions they caused release on `Fact::Integrate`
  (`Snapshot::release_member_leases`), which is now one stage later. A sibling
  evicted off a contended file waits for the judge, not the compiler.
- The member line's wall clock grows by one model lane per member. `Review` keeps
  the hour every model stage has; `GATE_WALL_CLOCK_SECS` is a gate's ceiling and
  a judge is not a gate.
- `MEMBER_LINE` changing is a catalog re-digest — a coordinated wire/catalog
  break of the same shape ADR-0153's was, with the same throwaway-journal
  position.
- **Open, and named here so it is greppable:** the ADR-0218 contextual and
  shared-run verification path mints its own member claim
  (`reduce::coordination`'s `MemberVerifyOutcome::PassedStandalone` arm and the
  `state.claims` table eager integration folds from) without passing through the
  dispatched line, so a bloom sealing `VerificationMode::Contextual` still
  integrates a member its judge never saw. Routing that path's green through
  `Review` is the follow-on this decision requires before contextual blooms
  inherit the gate.

## Alternatives considered

- **Keep the aggregate position only and raise the construct seat.** Rejected as
  the answer to this problem: it is the expensive half of the trade (every
  member's construction moves up a tier, not just its judgement) and it does not
  make any individual candidate reviewable before it folds.
- **Route a red member Review straight to `Refine` regardless of disposition.**
  Rejected: the bloom already seals an answer to "what is a member that did not
  pass worth", and a second, gate-specific policy would let one bloom eject on a
  compiler and bargain with a judge.
- **A new `WithdrawalCause::Review`.** Rejected: `WithdrawalCause::Verify`'s own
  documented reasoning is that the causes a reader cannot act on differently
  should not be separate discriminants, and which gate refused is already the
  first clause of the reason.
- **Reuse `Fact::AttemptCompleted { stage: Review, passed: false }`.** Rejected:
  it carries no findings, so the ejection reason would name only an evidence
  digest and the repair lap would be handed no work order.
- **Judge on the same seat that constructs.** Rejected: a judge that shares the
  producer's seat shares its blind spots, which is the whole reason the sealed
  bundles key `Review` separately from `Construct`.
