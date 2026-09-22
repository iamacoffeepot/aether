# ADR-0219: Bloomery admin mode

- **Status:** Provisional
- **Date:** 2026-09-15
- **Deciders:** iamacoffeepot

## Context

> "We need admin controls on bloomery to fix broken states like this where we can rerun it in admin mode, fix it, then exit out of admin mode"
>
> — the owner, 2026-09-15

The state that prompted it, exactly as it stood on the live coordinator.

Bloom `0f16e207`: eight members sealed; three withdrawn by the operator
(`issue-5969`, `issue-5978`, `issue-6025`); five integrated. The aggregate
verify passed over tree `74288367…`. The aggregate review (Sonnet critic,
`dispatch-7263`) came back red with findings that named the three withdrawn
members as "missing from the composed tree", because the review order still
listed their work orders. The reducer answered the red verdict the only way it
knows how — it dispatched a Refine lap on the composition (`dispatch-7265`),
which spent twenty-five minutes re-authoring three withdrawn members' work.

The operator's tools at that moment were `POST /blooms/{id}/hold` (freezes
further dispatch, but the running lap continues), `xtask bloom repair
--from-commit` (only for a *wedged* member or composition), `retry` (only the
member's current stage, and it spends a machinery roll), `withdraw`, `reverify`
(base only), `cancel`, and `reopen`. None of them could:

1. stop the running lap without it counting as a host fault and burning the
   retry budget,
2. discard that lap's output,
3. put the composition back on the tree that actually passed,
4. say "these review findings are void, the members they name were withdrawn",
   so the verdict counts as passed, or
5. re-run one gate on demand.

The repair the operator wanted to perform was a *sequence*, and every existing
door is a single decision the reactor acts on the moment it lands. That is the
right shape for a bloom that has stopped and the wrong shape for a bloom that is
running wrong: making the five moves one at a time against a live reactor is
exactly how a red review bought a model lap over members that had already left.

Two further facts about the existing vocabulary shaped what follows. The
operator hold (#4976 / #5100) already owns the one dispatch choke in the
reducer — every `DispatchAttempt` is built in one function, and the brake rides
on the value that is the only way to reach it. And the adjudication door (#4957)
already closes composition findings by naming their evidence digests, with
`BloomRecord::open_composition_findings` reading the closure.

## Decision

Add **admin mode**: a per-bloom session, opened and closed by an operator, in
which the machine dispatches nothing, faults cost nothing, and five repair acts
plus a read are accepted. Every act is journal-first — the REST edge appends a fact and
nothing else, and every state movement is the reducer's, so an admin session
replays exactly as it happened rather than as the current binary would
re-decide it (ADR-0190).

### The seven verbs

| verb | fact | what it may move |
| --- | --- | --- |
| `admin enter` | `Fact::AdminEnter` | sets the session flag; raises an `OperatorHold` if none is up |
| `admin exit` | `Fact::AdminExit` | clears the flag; releases the brake; dispatches what the cursors now owe; completes a waived join |
| `admin cancel-lane` | `Fact::AdminCancelLane` | cancels one named dispatch; nothing else |
| `admin set-candidate` | `Fact::AdminSetCandidate` | places a candidate at a gate position |
| `admin rerun` | `Fact::AdminRerun` | aims one workpiece at one stage |
| `admin waive` | `Fact::AdminWaive` | closes named findings; records the gate's pass |
| `admin drop-lap` | `Fact::AdminDropLap` | reverts one workpiece's candidate one lap |
| `admin status` | — | reads `/view`; writes nothing |

### Enter and exit

Entering records `Decision::RecordAdminMode` **and**, unless the bloom is
already braked, an ordinary `Decision::RecordOperatorHold`. Admin mode does not
add a second brake: it reuses the one #4976 built, so there is no dispatch path
that could be taught about a hold and not about a session. Over a bloom an
operator has already braked, the existing hold stands untouched — re-recording
it would overwrite the reason that operator gave, and refusing instead would
leave a gap between release and enter in which the reactor dispatches the very
laps the session exists to stop.

The flag is not redundant with the hold, because the two say different things. A
hold says *nothing is being dispatched*. The flag says *a person is in there
changing things*, and that second statement is what makes an executor fault free
and the five acts admissible. Both are projected: `BloomView::operator_hold` and
`BloomView::admin`.

Exiting clears the flag, releases the brake, and re-derives what is due through
the release's own helpers — every workpiece whose dispatch the session swallowed
from the cursor it sits at *now*, and every aggregate gate the session owes from
the fold the record is holding now. Nothing is stored at enter time to be
replayed, so a bloom the operator moved by hand resumes from where they left it.

### Faults cost nothing

Two named hooks, one per fault reducer:

- `reduce/attempt.rs`, in `reduce_member_executor_fault`, after the binding
  checks and before the roll is counted.
- `reduce/review.rs`, in `reduce_aggregate_review_executor_fault`, after the
  held-fold checks and before the fault series is extended.

Each asks `admin::absorbs_faults(record)` and, if so, returns
`admin::absorbed_fault(…)`: the evidence on the record, and nothing that moves a
budget or a cursor. A host that cannot run is the expected condition while an
operator is repairing a bloom by hand, and charging it would wedge the member
they are in the middle of fixing.

A cancelled lane costs nothing for a different reason: no fact is admitted for
it at all. `Decision::CancelLane` reaches the executor through
`Topic::CancelLane`, which cancels and consumes exactly one nonce — the
nonce-scoped sibling of the withdrawal path's `CancelDispatch`, which retires
every order a departing member holds. Model lanes are cancelled the way the
executor already cancels them; nothing kills a model CLI directly.

### What each act may and may not move

**`cancel-lane`** records the cancellation and journals it. No cursor moves, no
attempt or roll is spent. The nonce is *recorded* rather than validated by the
reducer, which cannot read the host's outstanding-order registry: the check
belongs where that registry is readable, so the executor drain re-resolves the
nonce and refuses one whose order names something other than this bloom and
workpiece. A nonce that no longer resolves is settled rather than re-driven —
the lap it named is already over, which is what the operator asked for. The
client resolves it a third time, off the live view, so a mistyped nonce reads as
"no live order is called that" rather than as a refused act.

**`set-candidate`** is the repair door without its wedge precondition. A member
lands at `Verify` carrying its spent counters forward — an operator writing the
candidate buys a lap, never a fresh budget, exactly as `OperatorRepair` does.
The composition's weave becomes the held integration, its cursor advances, and
the composite gates fall due. The candidate pair is derived from a commit and
its ref pushed with correspondence recorded, exactly as `repair --from-commit`
does; only the wedge precondition is dropped.

Placing the weave the record *already* holds is the incident's own move — the
operator putting the composition back on the tree its gates passed, after a
repair lap replaced it — and it is treated as what it is: a no-op on the fold.
No `RecordIntegration` is emitted, because that clears the composite-gate join,
and a gate that passed this exact tree has not stopped having passed it; only
the gates that have not passed fall due.

**`rerun`** aims one workpiece at one stage and spends nothing. It is not the
retry door: that one journals an executor fault, which is the operator asserting
the stage failed to *judge* its subject and is correctly charged a machinery
roll. Here the operator is asserting the record now says something different
from what the stage last judged — a withdrawn member, a replaced weave — so
there is nothing to charge. The runnable set is stated rather than read off the
catalog: `Verify`, `Review`, `Reconcile` for a member and the two composite
gates for the composition. `--now` lifts the session's own brake for that one
order and nothing else; the default defers, and exit dispatches it.

**`waive`** records three rows and no fourth: an `Adjudication` closing the
named findings, a `RecordAggregateGatePass` for the gate, and the admin act.
There is no synthesized verdict anywhere in that list. A reader of the journal
sees a red verdict, an adjudication naming its evidence, and a gate pass whose
only provenance is that adjudication.

**`drop-lap`** reverts one workpiece's candidate to the one it held before the
lap, keeping its stage and every spent counter. The lap's evidence stays exactly
where it is — a dropped candidate is still something that happened, and deleting
its verdict would leave the journal claiming a lap that never ran. The revert
target is read off `BloomRecord::displaced_candidates`, a one-deep undo folded
off the cursor, so the door does not have to trust the request for it. One deep
deliberately: the question it answers is "undo the lap that just finished", and
a deeper history would invite walking a bloom backwards through work its gates
have already judged.

### The honesty rules for waivers

1. **Evidence-bound.** A waiver names verdict artifact digests, and every one
   must be an open composition finding on this bloom or its bloom-scope park's
   own question. A waiver cannot void what was never raised.
2. **Adjudicated, not fabricated.** The ledger row is an `Adjudication` — the
   same value #4957 writes — so `open_composition_findings` needs no teaching
   about waivers and the closure sits beside the verdict it closed rather than
   replacing it. The gate pass recorded beside it also clears that gate's
   deferral, because a gate a person stood in for must not be re-dispatched on
   the way out.
3. **A review waiver needs only a reason.** A review waiver is a person
   overruling a *judgment*, which is exactly what an operator is for.
4. **A verify waiver needs `--i-know-this-lands-unverified-code`.** A verify
   waiver is a person overruling a *fact*. Refused without the flag.
5. **A landing that stands on a waiver says so.** Exit records an
   `AdminActKind::LandedOnWaiver` naming the head and the voided verdicts before
   the resolution effects, and `BloomView::waivers` carries the voided digests
   past the session's close, so the record still shows which red verdicts a
   person stood in for.

The land record rides the admin log rather than `LandingReceipt`. The receipt is
embedded inside `Decision::EmitReceipt`, which every frozen decision mirror
(`decisions_v1` and the `*_PRE_*` upcasts) decodes through today's live type;
giving it a field would force a full frozen copy of each mirror for a value the
admin log already carries.

### Why exit lands

A waived gate leaves a fold whose composite-gate join is complete and whose
completing verdict is never going to arrive, so nothing would resolve it — the
bloom would sit green and stationary. Exit finishes that join, under four
clauses, every one load-bearing: the bloom still holds a fold, both composite
gates have passed on it, no composition finding is open, and at least one of
those passes came from a waiver recorded in this session. Without the last
clause exit would race the ordinary verdict path; with it, exit resolves exactly
the folds a person stood in for.

## Consequences

- An operator can repair a broken bloom as a sequence rather than as five
  independent decisions against a live reactor, and pay nothing for the time
  they spend reading it.
- A bloom in admin mode is visible as such: `BloomView::admin` carries the
  operator, the reason, and the session's act log, and reaches the notification
  channel (`admin  bloom … is in admin mode (…): …`) and the console's interrupt
  list as its own `Admin` interrupt, which wins over the `Hold` it raised. An
  invisible session would be strictly worse than none, because the board would
  report a bloom as merely braked while a person was moving its cursors. The
  act log is what `admin status` reads, so an operator mid-repair answers "what
  has already been done here" from the projection rather than from a journal
  walk they would stop running.
- The reducer grows one module (`reduce/admin.rs`) and two one-`if` hooks in
  files it does not own. Everything else it reuses: the dispatch choke, the
  deferral tables, the release's owed-dispatch helpers, the adjudication ledger,
  the composite-gate join, and `resolution_effects`.
- Three `Decision` variants, seven `Fact` variants, and five `Outcome` variants
  are appended at their enums' tails, moving the `decisions` and `event` schema
  digests once. Both are pinned as `DECISIONS_PRE_ADMIN_DIGEST` /
  `EVENT_PRE_ADMIN_DIGEST` with upcasts that decode through today's decoder,
  because a tail-appended variant moves no discriminant a prior row could hold.
  `BloomView` gains two fields, so `ViewDocumentPreAdmin` joins the outbox-row
  upcast chain.
- The verbs are operator-authenticated the way every other bloom route is —
  which is to say, by the host-local bind and a mandatory non-blank reason and
  operator. Admin mode is not an authorization mechanism and does not stand in
  for an approval: the ADR-0181 re-check the two override doors make is made at
  every act inside a session, so a member whose sealed approval resolves above
  `auto` still needs its signed statement.
- A session left open holds the bloom indefinitely. That is the intended failure
  mode — a bloom nobody is dispatching is visible on the board, and the
  alternative (an expiring session) would hand the bloom back to the reactor
  mid-repair.

## Alternatives considered

**Ad-hoc SQL against the journal.** The fastest way to fix `0f16e207` on the
day, and the reason this ADR exists rather than a runbook. The journal plus the
recorded decisions *are* the truth (ADR-0149 / ADR-0190); a hand-written row
either bypasses the reducer that decides what a fact means — leaving a snapshot
no replay can rebuild — or duplicates it badly. It also leaves no audit trail
anyone can read afterwards: the one artifact an act no verdict produced has is
the record of who did it and why.

**More one-off verbs.** `cancel-lane`, `set-candidate`, `rerun`, `waive`, and
`drop-lap` could each have been a standalone door beside `hold` and `repair`.
Rejected because the reactor is live between them: the operator would cancel the
lane, and the reducer would immediately re-dispatch from the cursor the
cancellation left; they would set the candidate, and the gates would run over it
before the waiver landed. The session is the thing that was missing, and the five
acts are what it is for. It also would have meant five more places to remember
the fault-absorption rule, instead of one flag two hooks read.

**Killing lanes by hand.** `pkill` on the model CLI, which is what actually
happened to earlier incidents. It strands the order in the outstanding registry,
the deadline reaper eventually synthesizes a timeout, and the timeout admits a
failed attempt — so the operator pays a retry budget for a lap they cancelled.
It is also the documented way to reset a whole session tree by accident. The
executor already knows how to cancel a lane for a withdrawal; admin mode reuses
that path and narrows it to one nonce.

**Making the hold do it.** Widening `OperatorHold` with a mode flag rather than
adding a session. Rejected on the hold's own terms: #4976 states it is
"bloom-level and flat — no scope, no priority, no expiry", and the whole value
of that door is that it needs no policy to resolve against. A hold that meant
two different things depending on a flag would need one.

## References

- ADR-0149 — the control core; the journal is the truth.
- ADR-0153 / ADR-0191 — the composite gates, the composition's findings channel,
  and the two-pass ceiling that parks a bloom.
- ADR-0181 — a member's sealed approval binds its own subject.
- ADR-0187 — persisted-kind schema digests and their upcasts.
- ADR-0190 — replay folds recorded decisions, not re-decided facts.
- ADR-0205 — the coordinator is the only writer of the day.
- #4957 — the manager override: adjudication and operator repair.
- #4976 / #5100 — the operator brake and its aggregate half.
- #5327 — member withdrawal, and the lane cancellation admin mode narrows.
- #5423 — the retry door, and why a re-run is not one.
