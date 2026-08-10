# ADR-0176: Aggregate-review executor environment-fault lifecycle

- **Status:** Proposed
- **Date:** 2026-08-10

## Context

Bloomery's whole-bloom review distinguishes a critic finding from a critic that could not judge the candidate. The review lane already emits `VERDICT: environment` for the latter, but the xtask evidence envelope reduces both results to `status: fail`. The local executor therefore reports `StageVerdict::VerificationFailed`, intake admits `Fact::AggregateReviewCompleted { passed: false }`, and the reducer treats the infrastructure failure as a candidate defect. With an empty implication that fail-closed path revokes every member claim and sends every member through `Refine`.

That transition makes two bounded ledgers lie. ADR-0153's aggregate-review rolls count judgments of integrated candidates, and each member's repair rolls count attempts to correct a candidate. An executor environment fault is neither. Retrying it through either ledger can spend all candidate repair without any candidate having been judged.

ADR-0149 requires executor results to enter through a typed, digest-bound evidence channel and makes the journal the durable truth. ADR-0151 keeps the fact vocabulary closed and reserves `parked` for a pending human decision represented by a `Question`; a host outage is not such a decision. ADR-0153 freezes candidate findings, permits two aggregate-review judgments, and parks a second failed judgment. ADR-0174 makes the sealed `StageCatalog` the reducer's source for per-stage retry budgets. None of those decisions defines a durable lifecycle for a dispatched stage whose executor could not produce a judgment.

## Decision

Represent an aggregate-review executor environment fault as its own typed outcome from production through projection.

The review xtask stamps `status: environment` when its terminal verdict is `VERDICT: environment`. The executor transport exposes that as an appended `StageVerdict::ExecutorFault`, and normalization records an appended `EvidenceKind::ExecutorFault`. This means only that the dispatched lane could not judge its displayed subject; it does not assert that the subject passed or failed. In v1 intake accepts this verdict only for `StageId::AggregateReview`. Receiving it for another stage is refused rather than given unratified lifecycle semantics.

Aggregate-review intake admits a new appended fact, `Fact::AggregateReviewExecutorFault { bloom, evidence }`. The evidence must bind the exact tree of the integration fold currently held for review, under the same nonce and consume-once rules as every other executor result. It never becomes `AggregateReviewCompleted`, findings are not decomposed or persisted, and no candidate implication is inferred.

The reducer handles the fact on a branch separate from candidate review. For the held fold it records the fault evidence and increments a durable aggregate-review fault counter keyed to that fold's tree. It does not increment `aggregate_rolls`, clear the integration, revoke a resolution claim, move a member cursor, increment a member repair counter, or write review findings.

The numeric ceiling is the sealed `StageCatalog` binding's existing `retry_budget` for `AggregateReview`, but its ledger is independent from the aggregate candidate-review ledger. Thus each fault retry and each candidate judgment has an explicit bound without adding a second calibration field to the catalog. Below that ceiling the reducer emits a fresh `DispatchAggregateReview` for the same integration tree and head. The dispatch is a new order with a new nonce; replay of the admitted fault remains idempotent through its journal key. A different fold tree begins a new fault series at zero, while redispatch of the same held fold continues the durable series across process restarts.

When the counter reaches the sealed ceiling, the reducer emits no dispatch. It records a terminal bloom-scoped executor-fault state containing the fold tree, fault count, budget, and latest evidence digest. This is a wedge, not ADR-0151 question parking: there is no unanswered product decision to adopt, so no synthetic `Question`, pending-decision hold, or answer path is created. The integration and all member claims remain held. Recovery requires an explicit successor bloom after the operator repairs the environment; ordinary reactor polling cannot reset the counter or silently buy another attempt.

The journaled fact and its folded state are replayable. The self-contained `BloomView` projects the terminal fault, and may also project an in-progress fault series, with the subject, rolls, budget, and latest evidence. Logs remain diagnostics rather than lifecycle state. Existing candidate-review findings, repair re-entry, two-pass ceiling, question park, refused-admit logging, and repair-candidate propagation remain unchanged.

## Consequences

- An executor outage can no longer masquerade as a candidate finding or consume a member repair lap.
- Environment retry is bounded, survives restart, and is independently observable even though it uses the same sealed numeric calibration as the AggregateReview binding.
- The public verdict, evidence, fact, snapshot, outcome, decision, and projection vocabularies gain appended variants or fields. Trial journal and serialized projection compatibility must be handled explicitly by the implementation.
- The first implementation is intentionally aggregate-review-specific. Other stages must refuse `ExecutorFault` until a later decision gives them lifecycle semantics.
- A terminal fault deliberately requires supersession rather than an automatic reset or an ADR-0151 answer. This is conservative: repair of host health is operational authority, not candidate intent.
- Lane-boundary coverage must prove both redispatch below the ceiling and terminal observation at the ceiling, with no member Refine dispatch and no claim or repair-counter change.

## Alternatives considered

- **Continue encoding the result as `status: fail` and parse findings prose.** Rejected because lifecycle policy would depend on human-readable text after the typed distinction was discarded.
- **Admit a failing `AggregateReviewCompleted` with no implicated members.** Rejected because the reducer intentionally expands an empty implication to every member as a fail-closed candidate-repair rule.
- **Map the fault to `StageVerdict::Parked`.** Rejected because ADR-0151 parking represents a pending human decision and carries a `Question`; an executor outage is an operational fault.
- **Charge the candidate-review or member-repair counter.** Rejected because no candidate was judged and either choice falsifies its ledger.
- **Add another retry field to every stage binding.** Rejected for v1 because only AggregateReview has ratified environment-fault semantics; a separate durable counter using the existing sealed stage calibration is sufficient and leaves no unread catalog fields.
- **Retry forever or let the reactor's process-local backoff be the bound.** Rejected because neither behavior is journal-derived, replayable, or bounded.
