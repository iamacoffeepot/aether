# ADR-0180: Bloom retry grading over a journal-derived dispatch ledger

- **Status:** Accepted
- **Date:** 2026-08-11

## Context

ADR-0151 makes the study report a pure read: `grade` folds a bloom's admitted evidence into actual cost,
wall-clock, and retries, and grades each axis against the sealed `Forecast`. It names the retry axis'
source — "the admitted study records and the attempt history in the evidence log" — but never says what
one retry *is*.

`grade` (`crates/aether-bloomery/src/study_report.rs`) resolves that gap by counting every
`EvidenceKind::StudyRecord` entry in the bloom's evidence log and subtracting one. A study record is
emitted per attempt, and independent members and independent stages each produce their own, so a bloom
whose two members each complete one clean `Construct` grades as one retry, and a single member walking
`Construct` → `Verify` cleanly grades as one retry. The `retries_delta` against
`Forecast::predicted_retries` therefore measures "distinct executions minus one" for any bloom past one
member at one stage, which is not the quantity the forecast predicts.

Three forces bound the fix.

**The stage topology is ADR-0153's, not ADR-0149's.** The dispatched member line is `Construct → Verify`.
`Refine` sits off that line as the repair re-entry. `Review` binds no dispatched member stage. The
bloom-level positions — `Integrate`, `AggregateVerify`, `AggregateReview`, `Land` — each dispatch against
their own catalog budgets, and each can run more than once (a failing aggregate verdict re-opens members
and re-folds; a refused landing re-opens the line under `landing_rolls`). A retry definition has to name
which of those positions it counts and at what granularity.

**The counters that exist are headroom cursors, not history.** `StageProgress.attempts` resets to `1` at
every stage advance, so a member's `Construct` spend is gone the moment it reaches `Verify`.
`Fact::GrantAttempts` deliberately rewrites `attempts` and `repair_rolls` downward to leave exactly the
granted headroom under the stage's budget. An owner's answer to a bloom-scope review park resets
`aggregate_rolls` to zero to re-arm the cycle. No counter exists for `Integrate` dispatches at all.
ADR-0178's `seen_verify_failures` is genuinely per member and replay-stable, but it records verifier
identities rather than executions, and its companion `repair_rolls` is rewritten by the same grant path.
Every one of these is correct as a budget cursor and wrong as a ledger: a quantity an operator can hand
back cannot also be the record of what was spent.

**The journal is the only durable state.** `Commit.event` persists the encoded `Fact`; boot replay decodes
each journaled event, re-runs `reduce`, and folds the resulting decisions into a fresh `Snapshot`
(`crates/aether-chassis-bloomery/src/control/runtime.rs`). Nothing persists `Snapshot` or `BloomRecord`.
Any quantity derived inside that fold is therefore rebuilt for blooms already journaled, and any quantity
that needs a new `Fact` field is not.

## Decision

### The bloom carries a dispatch ledger

`BloomRecord` gains `dispatches: BTreeMap<DispatchKey, u32>` — journal-derived and replay-rebuilt like the
rest of the record. `DispatchKey` is a closed two-variant vocabulary naming the slot a dispatch targets:

- `Member { workpiece, stage }` — one slot per member per stage.
- `Bloom { stage }` — one slot per bloom-level position.

The reducer's six dispatch decisions become snapshot-folding: each increments its own key by one.
`DispatchAttempt` carries the workpiece and stage it dispatches and keys `Member`; `DispatchIntegration`,
`DispatchAggregateVerify`, `DispatchAggregateReview`, and `DispatchLand` each key `Bloom` at their own
stage. The outbox rows those decisions carry are untouched — only their snapshot fold is new.

### One retry is one dispatch of a slot beyond its first

```text
actual_retries = Σ over keys (dispatches[key] − 1)
```

A slot dispatched once contributes zero, which is the same beyond-the-first reading the stage retry budgets
already use. Independent slots never contribute to one another: two members each constructing once are two
keys at one dispatch each, and a clean `Construct → Verify` walk is two keys at one dispatch each.

### The counted positions are exactly the dispatched ones

Included: `Construct`, `Verify`, and `Refine` per member; `Integrate`, `AggregateVerify`,
`AggregateReview`, and `Land` per bloom.

Excluded, because no reducer decision dispatches them: `Sketch`, `Scope`, and `Approve` are pre-seal
operator and host positions; `Review` binds no dispatched member stage; `Study` is not dispatched as a
worker lane. A stage that acquires a dispatch decision later joins the ledger by construction rather than
by an edit here.

### Grants count, a parked attempt does not, and only a successor resets

- **A granted attempt is a retry.** `Fact::GrantAttempts` emits `DispatchAttempt`, so the ledger records
  it. The grant rewrites the headroom counters and leaves the ledger alone: the operator bought real
  execution, and the grade reports what was spent.
- **A parked attempt's release is not a retry.** The answer replays the held work order host-side under a
  fresh nonce and mints no reducer dispatch decision, so nothing reaches the ledger. ADR-0151's "parking
  consumes no retry — a decision pending is not a defect" holds structurally, with no exception to write.
- **A re-armed review cycle is a retry.** The bloom-scope review park is a spent ceiling rather than a
  parked attempt: its two verdicts already ran. The answer buys a fresh cycle, and the review it dispatches
  is a real execution, so it counts even though `aggregate_rolls` resets to re-arm the budget.
- **Only a successor resets the ledger.** A successor bloom is a distinct id with its own record and its
  own sealed forecast, so it starts empty. Nothing inside a bloom's life clears it.

### The retry axis stops reading study records

`grade` sums the ledger. Study records remain the source of the token and duration axes only, so an
unresolvable study artifact affects cost and time and cannot touch retries.

### This amends ADR-0151

ADR-0151 named the evidence log as the retry axis' source; this record replaces that source with the
dispatch ledger and settles what ADR-0151 left open — the unit, the counted positions, and the
grant/park/successor rules. The rest of ADR-0151 stands unchanged: the report is still a pure read over a
journal-derived snapshot, it still opens no port and mutates nothing, and its cost and wall-clock axes are
still the admitted study records summed.

## Consequences

- **No wire change.** No new `Fact`, no new `EvidenceKind`, no new `Decision`, no artifact-byte change.
  Journals already written replay into the ledger because the ledger is a fold over decisions the same
  facts already produce, and the pinned golden decision digest is unchanged because the decision stream is.
  This is the first bloomery grading change that costs the trial journal nothing.
- **The ledger cannot desynchronize from the dispatches it counts**, because it is a function of the
  dispatch decision rather than a second effect emitted beside it. A dispatch site added later is counted
  without being told to.
- **`BloomRecord` grows a bounded map** — at most three keys per member plus four bloom-level keys.
- **`Forecast::predicted_retries` becomes a number an operator can forecast against.** Zero predicted
  retries is now achievable by a clean multi-member bloom, which it was not.
- **A member's retry contribution can grow from a verdict it did not cause.** A failing aggregate verify or
  a refused landing re-opens every implicated member into `Refine`, and each of those re-entries is a
  dispatch of that member's slot. That is the intended reading — the axis measures work re-done against a
  forecast of work — but it means the per-slot counts are not per-member blame.
- **The outward view and the REST projection do not surface the ledger.** Whether an operator should see
  per-slot counts beside the wedge and cursor state is a separate question this record leaves open.

## Alternatives considered

- **Keep the bloom-global study-record count minus one** — first attempts on independent members and stages
  read as retries, which is the defect.
- **Derive from the existing budget cursors** — `attempts` resets at every stage advance, grants rewrite
  `attempts` and `repair_rolls` downward by design, an adopted answer zeroes `aggregate_rolls`, and no
  counter covers `Integrate`. A headroom cursor and a spend ledger cannot be the same field.
- **Emit a paired `RecordDispatch` decision beside every dispatch** — a second thing to remember at each
  site, enforceable only by a test that enumerates dispatch shapes, and it moves the pinned golden decision
  stream while carrying no information the dispatch decision does not already hold.
- **Carry the slot inside the `StudyRecord` artifact and group resolved records** — moves the content
  address of every study record, and makes retry history depend on artifact resolution, which ADR-0151
  refuses on the grounds that a grade computed from a side index is a grade the journal cannot replay.
- **Count failures rather than dispatches** — failure arrives through three different doors
  (`Fact::VerifyFailed`, `Fact::AttemptCompleted`, and the bloom-level verdict facts) with no uniform
  vocabulary, and a re-dispatch is what a forecast of retries actually predicts.
- **Reset the ledger on a grant or an adopted answer** — hides exactly the spend the operator authorized,
  which is the spend a forecast grade exists to report.
