# ADR-0205: The Coordinator Is The Only Writer Of The Day

- **Status:** Proposed
- **Date:** 2026-08-19

## Context

ADR-0199 moved source authority into the fleet repository and demoted GitHub to a one-way replica. ADR-0203 finished the job for the mainline advance: the roll writes `refs/heads/main` itself under compare-and-swap, and no pull request gates anything. Between them, the coordinator became the author of every ref write the estate performs — with one exception that was never decided, only inherited.

An operator still writes the day's branch by hand. An ADR commit, a documentation fix, a configuration change: the operator commits it in a fleet worktree and pushes `refs/heads/bloomery/daily/<date>` directly. Nothing records that the write happened, nothing verifies its content, and nothing coordinates it against work already in flight. The authorization for it is possession of shell access on the fleet host.

That exception has a measured cost. On 2026-08-19 an operator commit landed on the daily ref while a bloom was walking — a one-line ADR status flip, `docs/adr/0203-…md`, changing a single blob. Two members whose lane subjects had been cut before that commit produced candidates carrying the pre-flip content. The containment check diffs a candidate against the coordinator's current observed head, so it attributed the operator's change to those lanes as an edit outside their declared surface, naming a file neither had opened. Neither member could repair a finding whose cause was not in its work: one burned refine laps until it was ejected, the other lost its inherited resolution and re-constructed from scratch. A sibling member whose subject was cut after the commit diffed clean, which is what isolates the cause to the base rather than to any candidate.

The containment defect is filed separately (#5277) and should be fixed on its own terms — a per-member containment verdict belongs against that member's own subject. But fixing it only removes the phantom finding. A commit that arrives underneath in-flight members still leaves their candidates based on a tree that no longer matches the day, and the fold pays for that through the ADR-0189 reconcile. The deeper problem is not how the write is judged; it is that a second writer touches the day at a moment nothing chose.

Two other standing costs come from the same exception. #5260 exists because a ref advance the coordinator did not perform emits no replica topic, so the GitHub mirror silently lags until the next land. And an operator commit is the only content that reaches the day image without passing a verify gate — the 2026-08-19 ADR commit was checked by nothing at all.

## Decision

The coordinator is the only writer of the day's branch. An operator change is proposed into the journal and integrated by the coordinator at a moment it chooses.

- **The unit is a bloom with no members.** An operator change is not a new mechanism with its own queue, ordering, and failure modes. It is a bloom whose candidate is supplied directly instead of being constructed: no work order, no construct stage, no review seat. It integrates, lands, and replicates through the paths that already exist, and it appears in the journal, the ledger, and the doctor's invariants the same way any other bloom does.
- **Proposal is signed.** A queued change is a write channel to the mainline, so it carries an owner or operator signed statement over its content digest, verified before the coordinator will apply it — the same discipline commission approvals already use (ADR-0179). Possession of journal write access is not authorization to write the day.
- **The coordinator picks the moment.** A memberless bloom is admitted when no other bloom is active — between a land and the next seal. It is never interleaved with walking members, whose subjects would otherwise straddle it.
- **It is verified like anything else.** The supplied candidate runs the verify gates before it integrates. A documentation-only change has an empty code closure and the gates cost little; a malformed one is refused instead of landing on the day image.
- **The override is explicit.** An operator may still write the ref directly for an emergency, as a named override that the doctor reports as drift rather than as a supported path. The absence of a routine bypass is the point.

## Consequences

- The invariant "only the coordinator writes fleet refs" becomes true and enforceable. The doctor can assert that every commit on the day's branch corresponds to a bloom it authored; anything else is drift, which today is undetectable because it is also routine.
- #5260 is subsumed. Its whole premise is a ref advance the coordinator did not perform; once no such advance exists, the replica reconcile it proposes becomes a drift detector rather than a heal. That issue should be re-scoped to the detector, not implemented as written.
- Operator changes gain verification and an audit trail they have never had. What was authorized by shell access becomes a signed, journaled, replayable record.
- Operator changes lose immediacy. A change proposed while a bloom walks waits for the board to clear, which on a busy day is hours. This is the intended trade: the estate already treats "wait for a safe moment" as normal for every other kind of work.
- A change queued across a day roll must carry to the next daily branch rather than being applied to a branch that has closed. The roll is the natural place to re-target the queue.
- ADR commits are the most common instance and are the reason this decision exists at all, given the batched-ratification flow where an ADR is committed and work proceeds against it.
- The decision has a bootstrap wrinkle: the ADR regulating direct commits to the day's branch cannot itself arrive by the mechanism it describes. It is either the last direct operator push, or the first change pushed through its own implementation.
- Amends ADR-0199 and ADR-0203, which established the coordinator's authority over source and the mainline advance without naming the operator's remaining write path.

## Alternatives considered

- **Fix only the containment base (#5277) and keep pushing by hand** — rejected: it removes the phantom finding but leaves in-flight members based on a stale tree, the unverified content path, the unauditable authorization, and the replica lag. It treats the symptom that was visible tonight and none of the ones that were not.
- **A dedicated operator-commit queue with its own reactor** — rejected: a parallel path with its own ordering, retry, and replica semantics to keep correct, duplicating what the bloom lifecycle already does. A memberless bloom reuses machinery that is already exercised on every wave.
- **Forbid operator commits entirely and route every change through a commissioned member** — rejected: an ADR status flip or a one-line documentation fix does not want a work order, a construct lane, and a review seat. The cost would be paid on exactly the changes least in need of it.
- **Apply queued changes only at the day roll** — rejected: the roll is already the busiest moment in the day and a change proposed early would wait the entire day. Between-blooms is frequent enough to keep latency reasonable.
- **Advisory locking, where the operator checks for an active bloom before pushing** — rejected: a convention enforced by the person most likely to be in a hurry. Tonight's commit was made by an operator who knew the rule about mid-day merges to main and did not connect it to the day's own branch.
