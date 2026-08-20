# ADR-0206: A Decision Returns The Reason It Refused

- **Status:** Proposed
- **Date:** 2026-08-19

## Context

The coordinator's reducer is a pure function from a bloom record to decisions. When it emits no decision for a transition, exactly one guard returned early — and that fact is destroyed at the `return`. The reducer knows precisely why the estate is not moving, and says nothing.

On 2026-08-19 a bloom sat in `Sealed` with every member resolved, every blocker field null, and the outbox row for its integration marked delivered. Nothing was wrong with the record an operator could see. The fold had refused, and the refusal carried a complete English sentence naming the member and the reason:

> member `issue-5184` carries a claim but no candidate ref under this bloom or its predecessor; refusing to fold a set missing a member's work

That sentence was constructed, matched against, and dropped. Recovering it took an hour of reading the reducer, the reactors, and the journal, and comparing git ref namespaces by hand. The information had existed the whole time.

This is not one defect. It is the shape of most of what went wrong in the same wave. A containment check judged candidates against a base their lanes never saw, and no record said which base it used. A tripwire compared verifier names where it needed finding content, and nothing showed what it had compared. A day roll's coverage barrier was handed a literal green map, so it could not refuse, and no stored fact revealed that its input came from nowhere. Each is a decision whose inputs and reasoning were invisible after the fact.

Two constraints shape the fix. The first is that a second description of the decision path drifts: a hand-maintained explanation beside the code stops matching it and then lies confidently, which is worse for an operator than silence. The estate already resolves this elsewhere — `describe_component` reads the wasm's own `aether.kinds` custom section rather than a manifest maintained next to it, and that is why it can be trusted. The second is that this code is written by model lanes rather than by people. A convention that "every refusal should carry a reason" has no enforcement behind it, is invisible in review at the volume the estate runs at, and will decay.

## Decision

A decision cannot execute without producing its justification.

Every decision function at an operator-visible boundary returns an `Outcome<T>`: either the decision, or a `Refusal` naming the gate, the guard that stopped it, and the values that guard read. Guards are values rather than bare `if` statements. `Outcome` is constructible only by the gate builder, so a decision that skips its justification does not compile.

```rust
Gate::new("dispatch_member")
    .require("approval_present", || bloom.approval.is_some(), || reads![approval: bloom.approval])
    .require("subject_matches_ground", || bloom.subject == ground,
             || reads![subject: bloom.subject, ground: ground])
    .decide(|| Decision::DispatchMember { workpiece: bloom.workpiece.clone() })
```

Refusals are recorded on the record as they occur, not reconstructed on demand. A served explanation reads stored facts; a hypothetical question re-runs the same functions. There is no separate explanation path in either case.

The boundary is deliberate and narrow: this applies where an operator asks "why did this not happen" — member dispatch, fold, aggregate verify, aggregate review, land, and draft admission. Internal invariant checks keep ordinary control flow.

## Consequences

An operator asking why a bloom is not advancing gets an answer from the machinery that decided it, with the values it consulted attached. The hour spent on the 2026-08-19 fold becomes one request.

A vacuous gate becomes visible. A guard whose recorded read is the same constant on every evaluation, from a value nothing computed, is legible as vacuous the moment anyone reads a stored refusal — which is exactly the failure that let a coverage barrier pass by being handed its own success condition.

The explanation path stops being cold. Because refusals are written on the ordinary path rather than served by a route exercised only during incidents, the machinery that explains the estate is under load continuously, and a defect in it surfaces before it is needed rather than during the emergency.

Decision functions must be pure, and this is the real price. Effects interleaved into a decision make it un-re-runnable, which forces the hand-maintained explanation this decision exists to avoid. The reducer already satisfies this; reactors that mix effects into decisions must separate them before their boundaries convert.

Call sites grow noisier. Two closures per guard and a named string cost lines against an `if`. That cost is accepted for the boundaries named above and explicitly not paid elsewhere.

Storage grows. Refusals accumulate on records that previously carried none, and the retention question is real but small — a refusal is a gate name, a guard name, and a short list of rendered values.

Follow-on work this creates: the served `why` route over stored refusals; conversion of each named boundary, which subsumes the refusal-visibility half of the silent-fold defect; and a separate decision on level-ordered admission, which generalizes ADR-0205's rule that the coordinator is the only writer of the day into an ordering constraint over every level of change. That last one is deliberately not decided here.

## Alternatives considered

- **A served route that re-derives refusals on demand, with nothing stored** — answers hypotheticals but not "why did this stop four hours ago", and leaves the explanation path exercised only when something is already wrong.
- **A plain `Result<T, Refusal>` with early returns and no gate builder** — simpler and more readable, and sufficient in a codebase written by people who read review comments. It cannot enforce that a reason was named, which is the property that has to survive lanes with no memory of the convention.
- **Structured logging at each early return** — the reason reaches a log rather than the record, so it is neither queryable against a bloom nor visible to the console, and it drifts from the guard the moment either is edited.
- **A hand-maintained table of refusal reasons keyed by guard** — the drifting second description this decision exists to prevent.
- **Applying the discipline to every conditional in the estate** — pure overhead on checks nobody interrogates, and a version of this that spreads everywhere is a version that gets reverted.
