# ADR-0153: Whole-bloom review

- **Status:** Proposed
- **Date:** 2026-07-18

## Context

ADR-0149's member line runs Construct → Verify → Refine → Review, with a model review at the end of *every member's* line. The churn record of building the bloomery under the predecessor pipeline showed what per-unit model review costs at scale: 43% of merged PRs drew at least one change-request round, everything over ~900 added lines went multi-round, and a large share of the rounds were manufactured by the review process itself — serial finding discovery, reviewer variance between rolls, infrastructure ceremony — rather than by defects. Reviewer variance is structural: two competent reviewers surface overlapping-but-different finding sets, so re-running a review is a re-roll, not a confirmation, and the model-review sweep found no single model/effort point that dominates the variance away.

Two prior changes positioned this restructure. ADR-0152 landed candidate propagation and integration: members produce real candidate trees, and resolution folds them onto an integration branch whose head is checkout-able — so there is now a concrete integrated tree for a bloom-level review to judge. #3659 landed the failure-repair shape: a failing review re-enters Refine (findings-directed, ceiling-bounded) instead of re-rolling the critic on an unchanged candidate, with the findings channel (#3656) carrying the critic's prose to the repair prompt.

What remains wrong is the review's *position*. A per-member model review judges each candidate in isolation, pays the review cost once per member (machine decomposition multiplies members, so this tax scales exactly against the decomposition the system wants), and can never see cross-member integration defects at all. Meanwhile every member already gets a mechanical Verify against its actual candidate.

## Decision

### The member line ends at Verify

The dispatched member line becomes **Construct → Verify**. A passing Verify is the member's terminal gate: it mints the member's `ResolutionClaim` — the verification evidence already binds the exact candidate tree, which is precisely what `reduce_integrate` re-checks — and the member integrates. The per-member model Review is removed from the line; `Refine` also leaves the standing line and becomes what #3659 already made it in practice: the **repair re-entry**, dispatched only when a gate fails and a directed fix is the thing that can change the next verdict. A failing Verify within its budget re-enters Refine (carrying the mechanical failure output through the findings channel) and the Refine pass returns to Verify for the delta-confirm; the re-entry ceiling wedges the member as today. Members are cheap again: one model construction, mechanical gating, model repair only on failure.

### One model review, over the integrated bloom

The model review moves to the reserved `AggregateReview` position and runs **once per bloom**, after every member has integrated and the integration fold has produced the head: the critic checks out the integrated head and judges the whole diff against the sealed intent — every member's task, one review context. This is where cross-member defects are visible, where review cost is paid once regardless of member count, and where machine decomposition stops being taxed for splitting work. The review lane built in #3646 is position-agnostic and moves here unchanged; the fold driver dispatches it against the integrated head before the bloom may resolve, under a bloom-level order record (the same intake claim contract, keyed to the bloom rather than a member).

### Findings freeze, route, and confirm — then park

A failing aggregate review's findings are **frozen**: fingerprinted at the moment of the verdict, then decomposed and routed to the members that own them. Each named member re-enters Refine exactly once with its findings slice (revoking its claim; the bloom cannot resolve while any member is re-open), passes back through Verify, and re-integrates. The second aggregate review is a **delta-confirm** against the frozen finding set: it judges whether the frozen findings were resolved, not a fresh hunt — new findings discovered on the second pass do not extend the loop.

The ceiling is **two model-review passes per bloom**, hard. A bloom whose delta-confirm still fails — or whose findings are contested — **parks to the owner** as a pending decision (the ADR-0151 hold vocabulary, at bloom scope): the owner resolves it by answering, superseding, or abandoning. The machine never buys a third roll of the dice.

### What stays

Per-member mechanical Verify is unchanged and remains the integration gate. The construct-lane instruction text absorbs a self-review against the five review pillars before concluding (the construction standard — its own follow-up), which is where per-member review pressure goes instead of a dispatched critic. `StageId::Review` remains in the vocabulary for wire stability but binds no dispatched member stage.

## Consequences

- Review cost per bloom becomes constant in the member count — the decomposition tax is gone, which the machine-owned-decomposition direction requires.
- Cross-member integration defects become reviewable at all (today no review position can see them).
- Reviewer variance is bounded by construction: at most two critic rolls per bloom, the second constrained to the frozen finding set, then a human decision — never an open-ended finding exchange between agents.
- A member's resolution claim rests on mechanical evidence; the model's judgment concentrates where the integrated result exists. A defect Verify cannot catch and the aggregate review misses lands — the accepted trade, priced against the churn data; the construction standard and the verify-gate ratchet (recurring findings → instruction text → lints) are the compensating loop.
- The member line shortens (`MEMBER_LINE` = Construct → Verify, Refine repair-only): a wire/catalog break of the same coordinated kind as ADR-0152's, with the same throwaway-journal position.
- Multi-member blooms still gate on merge-based integration (#3653) before the aggregate position can see a true multi-member head; single-member blooms get the full new flow immediately.
- Follow-on work: the aggregate dispatch + bloom-level order record; the findings freeze/fingerprint/route vocabulary; the bloom-scope park; retiring the per-member Review bindings; the construction standard text.

## Alternatives considered

- **Keep per-member model review and add the aggregate pass.** Rejected: doubles model-review cost, keeps the per-member variance churn, and re-taxes decomposition — the data says the per-member position is where the waste lives.
- **Fresh full review on the second pass instead of a delta-confirm.** Rejected: an unfrozen second pass re-opens the finding exchange (reviewer variance guarantees a different set) and unbounds the loop the ceiling exists to close.
- **A reviewer panel (N parallel critics, vote) at the aggregate position.** Deferred, deliberately not foreclosed: the position and ceiling decided here are panel-compatible (a panel is one "pass"); whether a panel's marginal catch rate justifies its cost is a calibration question for the study loop, not an architecture question.
- **Human review of every bloom instead of a model aggregate review.** Rejected as the default: the owner's attention is the scarcest resource; the design routes to the owner exactly the contested residue the two-pass machine loop cannot settle.
