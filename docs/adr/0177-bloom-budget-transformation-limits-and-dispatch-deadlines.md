# ADR-0177: Bloom budget, transformation limits, and dispatch deadlines

- **Status:** Accepted
- **Date:** 2026-08-10

## Context

ADR-0149 put both a Budget and a Forecast into every sealed bloom and put Budget into every portable Transformation. The checked-in types do not implement that promise. Budget.token_ceiling and Budget.wall_clock_secs have no readers. Budget.retry_cap is read only when an operator grants attempts to an already wedged member; ordinary stage retries are bounded by the sealed StageCatalog. Every production Transformation constructor writes Budget::default(), and neither executor backend reads it.

Forecast is different: study reporting already sums admitted attempt tokens and duration and grades them after execution. It is an estimate, not an enforcement surface. The present names blur that distinction further: predicted_cost is actually a token count, and actual wall-clock time is a sum of worker durations rather than elapsed bloom time.

The missing enforcement is visible in the opposite direction. An outstanding order records the Transformation but no dispatch time or deadline. A lane that never exits can survive every coordinator restart because the reactor's only age is a process-local Instant used for a warning. No completion is admitted, so no stage retry advances and no wedge can be recorded.

This record amends ADR-0149's sealed Budget shape. It preserves the bounded-bloom objective through finite membership, a closed stage line, sealed retry budgets, and finite per-dispatch execution limits rather than retaining unread bloom-wide ceilings.

## Decision

Remove Budget from BloomDraft, BloomSpec, the REST draft patch, and the public value vocabulary. Remove BloomSpec.budget(). GrantAttempts is bounded only by the named stage's sealed StageCatalog retry_budget: a request is valid from one through that budget, and a zero request is invalid. The removed retry_cap was neither cumulative nor the ordinary retry authority, so keeping it under a new name would preserve a second, misleading bound without adding capability.

Keep Forecast as grading-only sealed data and make its units explicit. Rename its axes to predicted_tokens, predicted_worker_secs, and predicted_retries. The corresponding StudyReport axes become actual_tokens, actual_worker_secs, token_delta, worker_secs_delta, and retries_delta. Forecast overshoot is reported after execution and never kills or refuses a dispatch.

Define actual retries per durable execution key. A member-stage key is the bloom, workpiece, and stage; a bloom-stage key is the bloom and stage. The first dispatch for a key is an attempt and every later dispatch for that key is a retry, including operator grants and a return to Verify after Refine. A successor bloom starts new keys. Study evidence must retain enough stage and subject identity to compute this total instead of treating all study records in a bloom as one attempt series. Issue #3666 owns that follow-on repair.

Replace Transformation.limits: Budget with Transformation.limits: ExecutionLimits. V1 ExecutionLimits contains one field, wall_clock_secs. Add wall_clock_secs to each StageBinding, and copy the resolved binding's value into every Transformation constructor. A catalog is invalid when the value is zero or greater than the named implementation maximum of 86,400 seconds. There is no zero-means-unlimited mode: every dispatched worker is finite. Exact compiled-line values are calibration and may change by producing a new StageCatalog digest; the nonzero finite contract and maximum require another decision to change.

The execution deadline begins when the host durably records the outstanding order, so queue and startup delay consume the same sealed allowance as running time. Persist an absolute deadline in Unix milliseconds beside the order and the canonical Transformation bytes. It is journal-adjacent host state needed to recover intake, not reducer state. Restarts read the same deadline and never replace it with a fresh now-plus-limit value. GithubMirrorConfig.stale_warn_after_secs remains advisory and does not affect termination.

On every intake cycle, observe completion before expiry. If valid evidence is already available, admit it normally even when the wall clock has crossed the deadline. If the run is still queued or running and the deadline has expired, cancellation wins. ExecutorBackend.cancel becomes idempotent for a handle backed by an outstanding order: success means the run is cancelled, already terminal, or already absent after a prior successful cancellation; a transport failure remains retryable and leaves the order live.

After cancellation, write one deterministic content-addressed TimeoutRecord derived from the order nonce, displayed subject, stage, and deadline, then submit it through the existing nonce and displayed-digest intake boundary. A member-stage timeout is VerificationFailed and consumes the existing attempt or repair budget. An AggregateVerify timeout is VerificationFailed and follows its existing aggregate-verify retry/park gate. An AggregateReview timeout is ExecutorFault under ADR-0176 because no review judgment exists; it consumes that ADR's separate fold-fault ledger and never reopens members. Intake refuses a timeout for a stage that has no executor-dispatch lifecycle. The outstanding-order consume-once rule makes late worker evidence and repeated timeout handling harmless.

The timeout record is constructed before order consumption. A store or encoding failure leaves the order reachable for retry. A crash after cancellation is safe because the same expired order produces the same timeout record and idempotent cancellation result on restart. The reducer receives only an ordinary existing failure fact, or ADR-0176's aggregate-review fault fact, so deadline enforcement does not introduce a parallel retry authority.

This is one coordinated pre-1.0 wire break. BloomSpec, StageCatalog, Transformation, REST draft JSON, StudyReport JSON, and persisted outstanding-order rows change together. Old canonical bytes are not silently reinterpreted and content addresses are not rewritten in place. The Bloomery store schema version must refuse an old trial store that contains blooms or outstanding orders and require an explicit operator reset or export/recreate cycle; an empty store may migrate mechanically. No legacy outstanding order receives a fabricated dispatch time.

## Consequences

- Every surviving limit or forecast field has one named reader and one unit.
- Bloomery stops claiming a hard bloom-wide token or elapsed-time ceiling that it cannot meter.
- Every dispatched lane has a finite sealed timeout, and restart cannot renew it.
- A never-exiting member lane becomes one deterministic failed attempt and reaches the existing bounded retry/wedge lifecycle.
- AggregateReview timeouts remain distinct from candidate findings through ADR-0176. That arm waits on ADR-0176's vocabulary: until ExecutorFault exists, an expired aggregate-review order is reported and left outstanding rather than recorded as a member-charging verification failure, because the wrong ledger is worse than a deferred one.
- Operator attempt grants remain bounded by the same sealed stage catalog the reducer already uses; removing retry_cap changes only nondefault drafts that used it to narrow a grant.
- Forecast and study wire names become accurate, and #3666 must add the missing execution-key axes before retry grading is correct.
- Upgrading a nonempty pre-1.0 Bloomery store requires explicit trial-state recreation. The refusal is deliberate: preserving old identities while inventing limits or dispatch times would be false compatibility.
- Unix wall time is the durable cross-process clock. Large wall-clock jumps can expire work early; moving the clock backward can delay observation until it catches up, so deployments must provide a sane system clock and surface clock anomalies.

## Alternatives considered

- **Enforce the current bloom-wide token and wall-clock fields.** Rejected because there is no live token meter, bloom work may execute concurrently, and projecting one bloom duration onto every worker silently changes its scope.
- **Grade Budget alongside Forecast.** Rejected because two sealed estimates with overlapping axes would compete; Forecast already owns post-hoc grading.
- **Keep retry_cap as a bloom-wide ordinary retry ceiling.** Rejected because retry state is per stage and member, while the existing field only narrows one operator grant.
- **Treat zero execution time as unlimited.** Rejected because it recreates the never-terminating order and makes the bounded promise configuration-dependent.
- **Use stale_warn_after_secs or TrackedHandle.first_seen as the deadline.** Rejected because both are host-local advisory state and restart renews the Instant.
- **Consume the order before cancellation or timeout-artifact creation.** Rejected because a crash or store fault would lose the nonce without durable evidence.
- **Dual-decode old BloomSpec and Transformation bytes.** Rejected because their content addresses encode the old meaning and legacy outstanding orders have no truthful deadline origin.
