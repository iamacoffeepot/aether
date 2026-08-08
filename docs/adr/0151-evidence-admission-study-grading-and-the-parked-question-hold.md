# ADR-0151: Evidence admission, study grading, and the parked-question hold

- **Status:** Accepted (shipped — evidence, parked-question, and study flows in `crates/aether-bloomery/src/control/` and `crates/aether-chassis-bloomery/src/store/runtime.rs`)
- **Date:** 2026-07-16

## Context

ADR-0149's reducer consumes evidence through exactly one door: `Fact::Integrate` carries a per-member
`ResolutionClaim`, and nothing else an attempt returns ever reaches reducer state. The other three
evidence classes (`Approval`, `VerificationResult`, `ReviewFinding` —
`crates/aether-bloomery/src/values/mod.rs`) normalize into `Evidence` values at the intake broker but
have no reducer entry, so a failed verification or an open review finding is visible to a host-side
reader and invisible to the state machine that owns retries and stage progression. Two working slices
have now hit this wall independently and parked on it:

- The study half of the runner lane (#3525, split from #3523): ADR-0149's study stage grades actual
  cost, time, and retries against the sealed `Forecast`, but a per-attempt cost record admitted through
  the broker lands only in a rebuildable host-side index — the reducer never learns it, so a study
  report cannot be a pure read over the journal-derived snapshot.
- The parked-question construct (#3533, owner-directed): today's pipeline lets a mid-stage agent post a
  question and resume on the answer. In Bloomery an attempt can only succeed or fail; there is no way
  to say "this stage cannot proceed until a person decides," so a genuine mid-construct decision point
  either burns the retry budget or fails the workpiece.

Both parks are one fork: the `Fact` enum is deliberately closed ("a closed stage vocabulary compiled
into Rust", ADR-0149 §The line), so growing it is an ADR-tier decision, and growing it twice in
uncoordinated slices would produce exactly the ad-hoc variant sprawl the closure exists to prevent.
This ADR ratifies both extensions in one coherent shape.

One force is structural and decisive for the question construct: ADR-0149's prompt closure is
fail-closed — an instruction-capable slot in an assembled prompt must trace to a signed statement. An
answer to a parked question is the most instruction-capable input in the system, so the answer *cannot*
be a mirrored comment or an untyped reply; the architecture forces it to be a native
`Provenance::AuthorSignature` statement. The machinery for that already exists unchanged: the
observation→intent adoption rule (ADR-0149 §The boundary) and the `Statement` value
(`crates/aether-bloomery/src/values/statement.rs`).

## Decision

**One new reducer fact admits evidence: `Fact::AdmitEvidence { bloom, evidence }`.** Not one fact per
class — the `EvidenceKind` discriminant already separates verification results, review findings, study
records, and questions, and the admission semantics (bind to an exact subject digest, append to the
bloom's evidence log, update derived member state) are common. `Fact::Integrate` is unchanged and
remains the resolution-claim terminal; a `ResolutionClaim` keeps entering through it, not through
`AdmitEvidence`. Admitted evidence appends to a **per-bloom evidence log** on the bloom's record —
journal-derived, replay-rebuilt state, so "the journal is the only truth" is preserved. The intake
broker's provenance gate (nonce + displayed-digest match against the outstanding-order registry, #3502)
is unchanged and remains the only path in; the reducer re-checks the binding
(`Evidence::validates(subject)`) exactly as `reduce_integrate` does today.

**`EvidenceKind` grows two variants: `StudyRecord` and `Question`.** A study record is the normalized
per-attempt cost/tokens/turns/duration artifact (#3523's `StudyRecord` value is its `detail`); a
question is a parked attempt's decision request. Both are evidence *about* an attempt subject — neither
is intent, and admitting one never advances a workpiece toward resolution.

**The study report is a pure read.** `grade(snapshot) -> StudyReport` derives, per bloom, actual cost /
wall-clock / retries from the admitted study records and the attempt history in the evidence log, and
grades them against the sealed `Forecast`. To make the retry axis gradeable, `Forecast` gains
`predicted_retries` alongside `predicted_cost` and `predicted_secs` — sealed like the rest of the
forecast, graded like the rest of the report. No new port, no side effects: the report is a projection
any consumer (the REST surface, the mirror, the study stage's attempt) computes from the snapshot.
The host-side per-bloom study index (#3523) remains what it is — a rebuildable projection for cheap
lookup — and stops being the only place study data lives.

**Attempts gain a third terminal outcome: `parked`.** The attempt vocabulary becomes
succeeded / failed / parked. A parked attempt returns a `Question` artifact through the broker like any
other attempt product — the question states the decision needed, the options considered, and what is
blocked, and its manifest names the attempt's inputs so the decision context is auditable. Parking is
terminal for the *attempt* (the worker exits cleanly; no lease is held open) but not for the *stage*:
it consumes no retry from the stage binding's retry budget, because a decision pending is not a
failure.

**Admitting `Question` evidence records a pending-decision hold.** The hold is derived member state in
the reducer (part of the evidence log's fold, not a new fact): the member's stage is held — the
scheduler dispatches no further attempts for it — until the hold is released. A bloom with a held
member cannot resolve (resolution requires every member integrated; a held member cannot integrate),
but sibling members proceed normally.

**The answer is adopted intent, and only that.** An answer is a native `Statement` with
`Provenance::AuthorSignature` whose parents name the question's exact digest — the same adoption shape
ADR-0149 §The boundary defines for observations, reused unchanged; there is no new answer value type
and no new signing surface. Admission is a second ratified fact, `Fact::AnswerQuestion { bloom,
answer }` *(amended 2026-07-16: the original sentence routed the answer "through the existing native
intent path", but no generic intent door exists — each intent class is its own fact (`Seal`,
`Supersede`, `Land`), so the answer needs its own, and this amendment is the "another ADR" bar the
Consequences set for the next variant; found by the implement pass on the parked-question slice)*: the
reducer verifies the statement is instruction-capable (`verify_authority`) and that its parents name a
question digest currently holding one of the bloom's members, releases that hold, and emits the
re-dispatch decision carrying the answer in the held stage's input closure; the new attempt's prompt
manifest names both the question and the answer digests, so the audit trail shows why the retry
diverged from its predecessor. A statement failing either check is refused — answering is as narrow as
the hold it releases, and a generic adoption door stays unratified until a second adopter exists.
Who may sign an answer is key policy, not reducer logic: the owner's key, or a key the owner has
delegated stead authority to, exactly as approval statements work today.

**Projection out, answer in — asymmetric by design.** The question rides the existing `ViewDocument`
push to the outward mirror (a comment on the shadow issue, so a person sees it where they already
look), and the REST control surface (#3498) grows `POST /blooms/{id}/answer` accepting a signed answer
statement. A mirrored reply is at most an observation an author may adopt; a comment never becomes a
command (ADR-0149 §The boundary, unchanged).

## Consequences

- The reducer becomes the single audit point for *all* evidence classes, not just resolution claims: a
  verification failure, review finding, study record, or question is journal-recorded state, and
  host-side indexes over evidence demote to rebuildable projections. #3525 and #3533 unblock and scope
  against this ADR; #3523's shipped normalizer gains a reducer admission to feed.
- `Fact` grows two variants (`AdmitEvidence`, and `AnswerQuestion` per the 2026-07-16 amendment) and
  `EvidenceKind` two. Both remain closed enums; this ADR is the recorded ratification the closure
  demands, and the bar for the *next* variant is another ADR.
- `Forecast` gains `predicted_retries`, so sealing a bloom now demands a retry prediction — the
  forecast side of the study grade stops being partial.
- A parked workpiece holds its bloom open indefinitely until answered or superseded. That is the
  correct cost: the alternative (a timeout that auto-fails a pending human decision) reintroduces the
  unbounded-failure semantics blooms exist to eliminate. The pressure this creates (stale holds on
  abandoned blooms) is relieved by the existing supersession path, which releases holds with the rest
  of the bloom's claims.
- The construct-lane worker wrapper and the stage bindings' completion gates learn the `parked`
  outcome; the broker accepts a question artifact as a valid attempt product. Neither changes the
  transformation shape — a parked `construct.implement` is still the same portable typed command.
- The answer path adds no new trust surface: signing, adoption, and the native-intent door all exist;
  the REST answer route is a thin front over them.

## Alternatives considered

- **One fact per evidence class** (`AdmitVerification`, `AdmitReviewFinding`, `AdmitStudyRecord`,
  `AdmitQuestion`) — rejected: four variants with identical admission semantics, differing only in the
  discriminant `EvidenceKind` already carries; sprawl without information.
- **Study records stay host-side only** (the #3523 status quo as the end state) — rejected: the study
  stage's grade must be derivable from the journal alone (ADR-0149 §The bloom: the bounded promise is
  what makes the grade mean something); a grade computed from a side index is a grade the journal
  cannot replay.
- **A new `Answer` value type** — rejected: an answer is exactly an author-signed statement adopting a
  digest, which `Statement` + `Provenance::AuthorSignature` + `parents` already state; a parallel type
  would duplicate the adoption rule and create a second instruction-capable shape to defend.
- **Parking as a stage failure with a no-retry flag** — rejected: it burns the semantic distinction the
  study stage needs (a decision pending is not a defect) and makes the retry ledger lie.
- **A timeout on pending decisions** — rejected: auto-failing a human decision point reintroduces
  unbounded failure; supersession already provides the deliberate release valve.
- **Growing `WasmActor`-style dynamic dispatch instead of enum variants** — rejected: the closed
  vocabulary is load-bearing for replay determinism and audit (ADR-0149 §The control core); openness
  here is the thing being deliberately refused.
