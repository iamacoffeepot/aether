# ADR-0184: Bloom Calibration Is Measured from the Journal

- **Status:** Proposed
- **Date:** 2026-08-12

## Context

Every stage of the bloom line runs under a calibrated profile — a harness, a model id, and a reasoning effort (`StageCatalog::profile_of`, ADR-0149 §The line). The calibration is refinable without an ADR, and ADR-0174's registry makes a per-bloom override sealable and attestable. What does not exist is any measured basis for choosing those values. The compiled line moved from Claude to Muse on cost intuition plus one external benchmark; the Codex arm has existed since the harness axis landed and has never dispatched a live lane, so its column is empty; and effort tiers have never been compared on the same work at all.

The measurement axes already exist, recorded per attempt, as of the ADR-0177/0178/0180/0181 arc:

- The **resolved profile** — every journal `DispatchAttempt` carries its `Transformation.model: ResolvedModel` (harness, model, effort), so which agent actually ran is a journal fact, not an inference.
- The **failure mix, typed per verifier** — a failing member Verify names its exact failing verifiers (`VerifyFailureSet`, ADR-0178), including `verify.suppress` (ADR-0181), which makes suppression pressure — the quiet-failure signature that decides whether a model is usable at scale — a countable column rather than an anecdote.
- **Retries per execution slot** — the journal-derived dispatch ledger (ADR-0180) separates two members constructing once from one member constructing twice.
- **Cost** — `StudyRecord` carries the five token columns, turns, and duration per attempt, priced by the bloom's sealed `PriceTable`, which prices computed token counts and ignores every harness's self-reported dollar figure.

Nothing reads these across blooms. `study_report::grade` has no production caller, the REST surface renders neither it nor any aggregation, and `StudyRecord` deliberately carries no profile — the join lives in the journal, not the record bytes. Meanwhile comparable runs are hard to produce on the live repository: a work order can land only once, so "the same task under four profiles" is unrunnable against real mainline. The lane-boundary arc built the missing generator: the fixture GitHub backend (#4732) drives whole blooms through the real reactor handoff (#4831) against an in-memory repository, replayable from any base.

There is also data the journal will never contain: operator dashboard readings, provider benchmarks, external evaluations. The price table already settled how such figures relate to computed ones — they do not blend.

## Decision

Model and effort calibration is derived from measured runs, projected from the journal. Three mechanisms:

**The capability ledger.** A pure read over the journal and the study artifacts, beside `grade` in `aether-bloomery`: attempts joined per dispatch across resolved profile, typed verify outcome, dispatch-ledger retries, and study cost, then aggregated per `(harness, model, effort) × stage` cell. Each cell reports attempts, resolved members, rolls-to-green, failure counts per verifier identity (suppression pressure as its own column), micro-USD per resolved member, and worker-seconds. Every cell carries its sample count, and the projection ranks nothing: presentation-side thresholds may mark a cell insufficient, but the ledger's job is honest counts, not verdicts. The coordinator surfaces it over REST (`GET /calibration`), and the same route finally gives the study report a production consumer. No wire type changes: `StudyRecord` stays profile-free, because the journal already binds attempt to profile and a second copy could only disagree.

**Benchmark blooms.** Comparable runs come from replaying landed history against the fixture backend: a landed pull request supplies the work order (its issue text), the checkout (its pre-merge base), the bar (its landing CI), and a reference answer (its real diff). A benchmark run seals one bloom per profile cell — the sealed `ModelOverride` is the cell selector — over the same order and base, repeated for sample size, on a trial store the calibration host owns. Benchmark blooms never touch the live repository or its refs; the fixture repository is the whole world. Their journal rows flow into the same capability ledger, distinguishable by their trial store, so live operation and benchmarking share one measurement vocabulary.

**Provenance classes.** The ledger distinguishes journal-derived rows (attested by the machinery that ran them) from carried-in annotations (dashboard readings, provider benchmarks, external evaluations). Annotations may sit beside measured cells for context; they never sum into a measured column. This is the `PriceTable` rule — computed and reported figures never blend — applied one level up.

The ledger measures gate-visible failure only, and says so. ADR-0181 made suppression countable; shared-artifact degradation (the second quiet failure class) still has no gate, so no ledger column claims to measure it. A calibration read is evidence about what the gates can see, and its caveat line is part of the projection, not documentation around it.

Calibration updates remain deliberate acts: the ledger informs an operator (or a bloom whose work order says so) editing `profile_of` or authoring a catalog, and the edit re-digests the catalog as today. No mechanism reads the ledger and changes a profile automatically.

## Consequences

- `grade` and the study machinery gain their first production consumer; the "cost ledger accumulates and nothing grades it" gap closes.
- Choosing a stage's model or effort becomes an evidenced edit: the catalog change cites ledger cells instead of intuition.
- The Codex column starts empty and visibly so — a smoke bloom and sealed price rows for its models are the entry fee, and the ledger's sample counts make "we have no Codex data" a rendered fact rather than tribal knowledge.
- The fixture backend acquires a second consumer beyond tests, which forces the trial-mode packaging decision: promote it to a first-class calibration mode or ship the calibration host as a testing-featured build. That decision is follow-on work, not made here.
- Benchmark tasks drawn from landed history age with the codebase: a replayed order references the tree as it was, so cells are comparable within a benchmark set, and sets are versioned by their base, not silently mixed.
- The ledger inherits the journal's honesty boundary: a harness that under-reports token columns produces cheap-looking cells. The sealed price table bounds this (unpriced is distinct from free), but the caveat belongs in the rendered output.

## Alternatives considered

- **Carry the resolved profile on `StudyRecord`.** Rejected: a wire break to duplicate a fact the journal already binds per attempt; two copies can only agree or be a bug.
- **An asserted calibration table (a checked-in ranking).** Rejected: it is the current state minus honesty — the compiled line already asserts a choice, and a second assertion layer would still measure nothing.
- **Automatic calibration (the ledger feeds the catalog directly).** Rejected for now: a feedback loop from measurement to configuration invites optimizing what the gates can see, exactly the blind spot ADR-0181 documents; deliberate edits keep a human judgment between the two.
- **Benchmarking against the live repository.** Rejected: a work order lands once, so live runs cannot produce same-task cells, and benchmark churn would pollute real history.
- **Blending external benchmark data into measured cells.** Rejected: the price-table rule — a confident blended figure is worse than two labeled ones, because nothing downstream can tell which half to trust.
