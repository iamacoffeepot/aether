# ADR-0189: Fold conflicts dispatch a reconcile stage

- **Status:** Proposed
- **Date:** 2026-08-13

## Context

A bloom's members construct in parallel, isolated lanes against one sealed base; the integration fold then merges their candidates sequentially onto the accumulated tree. When two candidates touch the same regions of the same file, the later member's merge conflicts — deterministically, since the same trees produce the same conflict on every attempt. The integrate reactor is correct to refuse rather than re-drive (`"the collision needs a decision, not a re-drive"`), but the refusal is a reactor-side log line: no fact enters the journal, the bloom stalls with every member resolved and verified, and the only path forward is a hand-driven supersession that discards a verified candidate.

The first production firing (bloom `87ed163e3ce5`, 2026-08-13) had six members resolved, five folded clean, and the last refused on a two-member collision in one file. The salvage — supersede carrying five, drop one, re-run it after land — works and loses little, but it routes a mechanical situation through the operator. The pipeline's own model lanes are competent to resolve merge conflicts; every interactive agent workflow in this repository already does so as a matter of course, under a stated contract: preserve both intents, stay inside the approved surface, prove the result again. What is missing is a pipeline stage that owns that contract.

Two properties of the existing vocabulary make the addition small. `StageId` is a closed, macro-generated list; the catalog maps over `StageId::ALL`, so a new stage cannot be silently unbound (`CatalogError::UnboundStage`), and `ModelOverride::per_stage` keys on `StageId`, so routing falls out. And only three dispatched model-lane commands exist — `construct.implement`, `review.critic`, `verify.check` — with `construct.implement` already doing exactly what conflict resolution needs: implement a work order in a worktree, capture the diff as a candidate.

## Decision

Cross-member fold conflicts become journaled facts that dispatch a **`Reconcile`** stage — a first-class member of the stage vocabulary, symmetric with its siblings.

1. **The conflict is a fact.** The integrate reactor stops refusing in prose: it admits `FoldConflict { bloom, workpiece, checkpoint, evidence }`, where `checkpoint` names the folded tree the candidate collided with and `evidence` carries the conflicting paths. Replay reproduces the state; the reactor holds nothing the journal does not.
2. **`Reconcile` is a stage, not a special case.** A new `StageId` variant, appended to the closed vocabulary, bound in `StageCatalog::line()` like every sibling: its own attempt limit, its own `per_stage` override key, the standard evidence envelope, the standard study row. Its dispatched command is `construct.implement` — the lane machinery is unchanged; only the inputs differ.
3. **Two honest asymmetries, stated once.** The lane's working tree is the *folded checkpoint*, not the sealed base; and the stage is dispatched by the `FoldConflict` fact, not by line progression. Everything else — capture, verify, review eligibility, retry accounting — is the ordinary member loop.
4. **The work order carries both intents.** The member's original description, its conflicted candidate's diff, and the conflicting paths, under the standing contract: reproduce this member's intent on top of what the fold now contains; stay inside the declared surface; the result faces the same verification as any candidate. No skill text is wired into the lane; the contract lives in the work order the host assembles, as it does for every stage.
5. **The reconciled candidate rejoins the ordinary line.** It verifies (`Verify`), replaces the member's resolution, and the integration re-drains and folds it. Exhausting the catalog's `Reconcile` attempts wedges the member with the conflict evidence attached — the existing wedge vocabulary, no new terminal state.
6. **Textual auto-merge is rejected as a mechanism.** Union/ours/theirs strategies produce silently broken code in a compiled language; the fold's only resolution mechanism is the dispatched lane, or the wedge.

## Consequences

- Declared-surface overlap between members stops being an operational hazard and becomes priced work: the seal door warns (the sibling overlap-warning decision), the fold absorbs what parallelism cannot avoid, and only a genuinely unresolvable conflict reaches the operator — as a wedged member with evidence, not a stalled bloom with none.
- Fold order acquires stated cost semantics: the later-folding member absorbs reconciliation. This matches the current refusal ordering and is deterministic from the sealed member order.
- The journal gains a fact and the stage vocabulary a variant — both appended, inside the positional format's additive window (ADR-0187's fast path; old bytes decode unchanged). A sealed pre-existing catalog remains valid for its own bloom; new seals require the new binding by `UnboundStage` refusal.
- Reconcile attempts enter the study ledger like any stage's, so the cost of overlap becomes measurable per bloom rather than folklore.
- Costs, accepted: one more stage to bind, budget, and reason about; a second dispatch trigger (fact-driven) beside line progression; and reconciliation spend that a stricter no-overlap seal policy would avoid — the trade is parallel throughput on a shared crate against occasional single-member resolution laps.

## Alternatives considered

- **Refuse overlapping seals outright.** Prevention only; coarse surface globs over-predict (members sharing a crate glob routinely fold clean), so the door would either over-refuse or under-protect. The door warns (sibling decision); it does not decide.
- **Operator-driven supersession, permanently** (status quo). Discards verified work, stalls the pipeline on a human, and routes a mechanical merge through the most expensive decision channel available.
- **Textual merge strategies in the fold.** Rejected above; restated here because it is the tempting cheap version — it converts a loud conflict into a quiet defect.
- **Serialize overlapping members into waves** (construct the later member against the earlier's folded tree from the start). Removes the conflict class but requires per-member bases in the sealed spec and fold-state-aware lane scheduling — a topology change an order of magnitude larger than a stage, for a class of conflict the stage handles at the same end state.
