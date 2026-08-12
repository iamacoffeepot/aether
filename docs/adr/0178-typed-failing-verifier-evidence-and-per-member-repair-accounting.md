# ADR-0178: Typed failing-verifier evidence and per-member repair accounting

- **Status:** Accepted
- **Date:** 2026-08-10

## Context

A member's terminal `Verify` stage runs the `verify.check` umbrella. The worker knows which of its six checks failed, but the evidence and journal contracts collapse that result to one pass/fail bit plus human-readable findings. The reducer therefore spends one `repair_roll` for every failed umbrella run. A member that discovers formatting, lint, and documentation defects in three successive rounds exhausts the same budget as a member that repeats one unchanged defect three times.

The intended budget measures stuckness, not the breadth of a candidate's first diagnostic pass. The first failure of each verifier identity should be forgiven for that member, while a later failure of an identity already seen by the same member should consume one repair roll. This requires typed failure identity at the executor boundary and in the journal. It also requires replay-stable per-member memory and a deterministic answer for umbrella runs in which several checks fail together.

ADR-0149 keeps retry and wedge decisions in the pure reducer. ADR-0151 makes evidence admission the trust boundary and keeps the fact vocabulary closed behind another decision. ADR-0152's candidate field established that appending a field to an existing fact variant changes its canonical encoding. ADR-0153 introduced the `Verify`/`Refine` repair loop, and ADR-0174 made the sealed stage catalog's `Verify` retry budget its ceiling. None of those decisions defines typed verifier failures, a preflight identity, or repeat accounting.

The two executor backends expose different evidence shapes. The local backend reads `evidence.json`; the GitHub backend currently observes an artifact name without downloading its body. The contract must produce the same admitted failure set on both paths without parsing diagnostics prose.

## Decision

### Closed failure vocabulary

Introduce `VerifyFailure` as a closed typed vocabulary with seven V1 identities, in this canonical order:

1. `verify.preflight`
2. `verify.fmt`
3. `verify.clippy`
4. `verify.docs`
5. `verify.test`
6. `verify.dup`
7. `verify.deps`

`verify.preflight` represents failure of the umbrella's prerequisite check before any verifier member runs. Missing program and target names remain diagnostic detail; they do not become unbounded accounting keys. A failed member preparation step belongs to that member's identity. New verifier identities require another ADR because the vocabulary size changes the maximum number of forgiven rounds. [ADR-0181](0181-suppression-verifier-identity-and-vocabulary-saturation.md) is that decision for an eighth identity, `verify.suppress`, appended to the canonical order; it also records that the eighth exhausts the mask byte.

`VerifyFailureSet` is a deduplicated set ordered by the list above. Its canonical serde form is an array of canonical identity strings. Internally it may use a mask over that order — eight bits wide since [ADR-0181](0181-suppression-verifier-identity-and-vocabulary-saturation.md) appended the eighth identity and assigned the byte's last bit, which leaves no unknown bit for a decoder to refuse. Unknown strings, duplicates, an empty set on a failed Verify result, and an out-of-order encoded set are refused at the trust boundary. The empty set is valid only for cursor initialization and non-Verify wedge projection. Human findings remain independent diagnostic prose and are never an accounting input.

### Executor and admission contract

The `verify.check` evidence record carries `failed_verifiers`. It is absent on a pass and required, non-empty, and exact on a failed run. The worker runs every member after a successful preflight, so the set contains every member whose preparation or command failed in that run.

Both executor backends project that field into `EvidenceRef`. The local backend decodes it from the evidence body. The GitHub workflow adds a canonical two-lowercase-hex-digit eight-bit mask to the artifact name, and the GitHub backend validates and decodes that mask; the artifact's content digest continues to bind the evidence bytes. Intake requires the body-derived and name-derived forms appropriate to each backend to agree with the verdict, nonce, subject, and digest checks already performed. A non-Verify result must not claim verifier failures.

Append a new journal fact variant `Fact::VerifyFailed { bloom, workpiece, evidence, failed_verifiers }`. Failing member `Verify` results are admitted through this variant; `Fact::AttemptCompleted` remains the fact for the other member-stage completions. The stage is implicit and intake still checks the outstanding order is the member's current `Verify` dispatch. Appending a variant preserves all existing fact discriminants and avoids changing the encoding or meaning of historical `AttemptCompleted` values. The coordinated pre-1.0 journal fixture and golden trace advance to include the new variant; stores with incompatible non-empty trial data refuse to open under the repository's existing compatibility policy.

### Per-member repeat accounting

Add `seen_verify_failures: VerifyFailureSet` to `StageProgress`, with an empty value permitted only as cursor state. For one admitted `VerifyFailed` fact with current set `F` and prior seen set `S`:

- compute the repeated set `R = F ∩ S`;
- update the member's seen set to `S ∪ F`;
- if `R` is empty, spend no repair roll and re-enter `Refine`;
- if `R` is non-empty, spend exactly one repair roll for the whole umbrella verdict, regardless of the size of `R`;
- if that roll reaches the sealed `Verify` retry budget, wedge the member instead of dispatching another repair.

The member key is the cursor inside one bloom record. Sibling members never share the set. It survives `Verify → Refine → Verify`, journal replay, and `GrantAttempts`; a grant restores numeric headroom but does not make a repeated verifier novel. A superseding bloom constructs fresh member cursors and therefore fresh seen sets. Candidate replacement within the same member does not reset the set.

The loop remains bounded. Let `N` be the closed vocabulary's identities — eight since [ADR-0181](0181-suppression-verifier-identity-and-vocabulary-saturation.md) appended `verify.suppress` — and `B` the sealed `Verify` retry budget. At most `N` failed verdicts can have an empty repeated set, because each must add at least one previously unseen identity; at most `B` later verdicts can spend rolls before the member wedges. A verdict containing both new and repeated identities spends one roll because its repeated set is non-empty.

### Wedge and projection

When repeat accounting exhausts the budget, the stored `Wedge`, reducer outcome, and outward view carry `repeated_verifiers = R`: the deterministic set of identities in the terminal verdict that had already been seen. Non-Verify wedges carry an empty set. The evidence digest remains the pointer to full diagnostics. The bounded typed set is suitable for operator display and does not expose arbitrary tool output in reducer state.

## Consequences

- Converging members receive one forgiven failure per finite verifier identity; repeated failures consume the existing sealed budget.
- Several repeated failures in one umbrella run consume one roll, so executor ordering cannot change accounting.
- Replay, local execution, and GitHub Actions derive the same decision from typed data rather than findings prose.
- `VerifyFailure`, the evidence reference, the appended fact, cursor state, wedge projection, Actions artifact name, trial journal fixture, and golden trace are coordinated pre-1.0 wire changes.
- The GitHub artifact-name parser becomes a validation boundary for the compact mask. The evidence body remains content-addressed, and findings remain advisory.
- #4683 must become a pure umbrella. One child owns typed worker production and local/GitHub transport through admission; a second child owns the journal fact, per-member accounting, grant behavior, wedge projection, and reducer invariants. The transport child lands first.
- Preflight is deliberately one synthetic failure identity. This keeps the loop finite and visible but does not turn a missing host dependency into a candidate-owned tool defect; richer executor-environment recovery would require a separate decision.

## Alternatives considered

- **Parse verifier names from findings prose** — prose is optional, truncated, backend-dependent, and not a typed replay input.
- **Use the umbrella command or evidence digest as the identity** — `verify.check` cannot distinguish members, while a content digest changes across runs and may contain several failures.
- **Add a field to `Fact::AttemptCompleted`** — it would alter the canonical encoding of every existing value and retain a stage-polymorphic variant whose field is valid only for one failure path; an appended fact is narrower and preserves prior discriminants.
- **Spend one roll per repeated identity in a multi-failure run** — one executor verdict is one repair decision; charging by set size would make a broad diagnostic run consume several rounds at once.
- **Reset seen identities on `GrantAttempts`** — a grant adds bounded numeric headroom; erasing history would redefine a known repeat as a first failure and add hidden forgiveness.
- **Carry each missing executable or target as an identity** — host-specific strings would make the accounting vocabulary unbounded and allow novelty to extend the loop.
- **Treat preflight as `ExecutorFault` under ADR-0176** — ADR-0176 deliberately admits that verdict only for `AggregateReview` in V1. Extending it to member Verify is a separate lifecycle decision.
