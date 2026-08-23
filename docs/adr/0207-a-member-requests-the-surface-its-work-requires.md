# ADR-0207: A Member Requests The Surface Its Work Requires

- **Status:** Proposed
- **Date:** 2026-08-19

## Context

A member's declared surface is fixed when its bloom seals. It is a prediction, written before the work exists, about which files that work will touch. The containment gate then refuses any candidate that reaches outside it, which is the guarantee the approval rests on: a signed scope revision names a surface, and nothing lands outside it.

When the prediction is wrong, the estate has no way to say so.

On 2026-08-19 a member demoting surface-derived edges from dispatch gating declared `crates/aether-bloomery/src/**`. Its own scope named the work three times as happening "at seal admission", and seal admission is `crates/aether-chassis-bloomery/src/api/runtime/seal.rs` — a file the surface excluded. The two tests pinning the behaviour the member was removing lived in that file. The work could not be completed inside the surface that authorized it, and the scope document contradicted itself in writing before anything ran.

What followed is the part that matters. The lane produced a correctly reasoned refusal:

> The last candidate failed because it touched `crates/aether-chassis-bloomery/src/api/runtime/seal.rs`, which is outside this member's declared surface. That file is already correct on this base. I am not editing it. There is no remaining authorized change inside `crates/aether-bloomery/src/**`.

That is a complete request: a path, a reason, and the evidence behind it. The estate scored it as "no candidate produced", burned an attempt, and dispatched the same lane again. It said the same thing. The attempt budget drained, the member wedged, and the wedge presented as a lane that could not get past a verifier — which invited a grant of more attempts, the one remedy that could not help, because the member did not need attempts. It needed permission.

Recovery took a hold, a hand-authored scope revision, a re-sign, a re-approval, and a supersede of a bloom whose other six members were already resolved. Every step was mechanical and none of it was decided by anything the machinery knew.

Two constraints shape the fix. The first is that surfaces will keep being wrong, because scoping predicts work that has not been done; a check at seal tightens the prediction but cannot remove the error. The second is that widening a surface is widening the boundary an approval attests, so it cannot be a side channel — a mechanism that lets a member reach further without touching the signed artifact destroys the provenance the containment gate exists to produce.

## Decision

A member that cannot complete its work inside its declared surface requests the paths it is missing, and that request is a first-class outcome rather than a failure.

**The lane returns it.** Alongside a produced candidate and no candidate, a lane may return a surface request naming exact paths, the reason the work cannot complete without them, and the containment refusal that prompted it:

```rust
SurfaceRequested {
    paths: Vec<String>,     // exact paths, not globs
    reason: String,         // why the work cannot complete without them
    evidence: DispatchId,   // the refusal that prompted the request
}
```

**The member parks in `AwaitingSurface`, not `Wedged`.** These are different conditions and must be different states, because they invite different remedies. A wedged member is one the machinery could not push through; a member awaiting surface is one waiting on a person. It costs nothing while it waits and it is visibly blocked on a decision rather than on a lane.

**An operator decides every widening.** A declared surface is the boundary an approval attests, so nothing the coordinator runs moves it: the machinery's whole part is to park the member and state the request it is parked on. The answer is `cargo xtask bloom amend`, which an operator runs and signs. The requested paths resolve through `approval-policy.toml` exactly as a scope revision's surface does, and the amendment's tier is the most restrictive tier over the *added* paths — applied there, before anything is signed, as the ceiling that operator's `--accept-tier` may not exceed. A `judge` delta goes to the judge seat and a `human` delta stops at the owner's desk with an owner-signed `Statement`, the same as any human-tier surface. The ladder therefore bounds how far an operator may answer unattended; it never authorizes the estate to answer in nobody's name. Human approval of a surface amendment is not a new trust mechanism either — it is the delegation ladder applied to a delta.

**An amendment authors a scope revision.** It re-scopes the member's commission with the added paths, signs, approves, and pins the member at the resulting revision. The commission remains the authority on what surface was authorized, and the audit trail carries the widening, its author, and its diff. An amendment that lived only in bloom state would leave the commission and the bloom disagreeing about what was approved.

**The widened revision reaches the member through a supersession.** A sealed spec pins a revision and a sealed membership is immutable, so the member cannot be re-pinned where it stands. The amendment supersedes the bloom onto a successor carrying every other member at the revision it already holds — inheriting its claim and its finished work — and the amended member at the widened one. The successor's own seal is the check that the widening is real: each member's commission must produce the revision it is pinned at and carry an approval of it, so a surface nobody stored and nobody approved cannot enter a bloom. The command only ever unions, so a member's surface grows or stays where it was.

**The work order is rendered from the revision, not carried alongside it.** The text a lane reads is the revision's stored description, and an amendment carries every field but the surface across unchanged — so the declaration block inside that text is re-rendered from `declared_surface` wherever an order is written. A lane handed back the surface it just declined against declines again, and that second request names paths its own revision already covers, which normalizes to nothing and leaves the member parked with no remedy stated.

**Amendments are budgeted.** A member may request a bounded number of times per bloom, requests name literal paths rather than globs, and each must cite a specific prior containment refusal.

## Interaction with declared reads and writes

ADR-0204 gives member declarations a read/write distinction, and the scope vocabulary carrying it puts two optional lists inside the surface: the files a member expects to write, which pre-seed the lease table, and the interfaces it load-bearingly reads, which derive conditional ordering against a co-member that actually writes there. Neither list is authority. The surface globs remain what containment refuses against, and they are what this decision amends — so the tier ladder over the added paths is unaffected by the split, and no amendment can escalate a read into a write, because neither is a permission.

An amendment extends the write-file list alongside the surface. The newly authorized paths are exactly the ones the member is about to write, so omitting them would seed the lease table with a known gap. This is a should rather than a must: a write lease forms at first observed write regardless, so a grant that omits the hint costs a late lease rather than a failure.

An amendment makes a write lease appear later than the sealed graph anticipated, which is the conditional-ordering trigger doing its job. Two of its three cases are already specified: a reader that has not yet dispatched takes the ordering as an edge, and a reader already dispatched takes it as a rebase at integration. A mid-bloom amendment introduces the third — a reader that has already resolved — and it settles like the second, by rebasing at integration, because a resolved member's candidate is not yet folded. A widening that would reorder a member already folded into the composition is refused, and the day re-scopes rather than amends.

The two mechanisms measure opposite halves of one question. Write-file declarations state what is nameable at seal time, and the measurement behind them found that a substantial fraction of out-of-core ripple files are not nameable in advance. A journaled surface request is that unnameable fraction becoming visible at construct time with a path and a reason attached, so the request log is the corpus from which better declarations get written. The two should land close together for that reason rather than being sequenced arbitrarily.

## Consequences

The information the lane already produces stops being destroyed. The estate's most common recovery — an operator reading a transcript, inferring what the lane needed, and re-authoring a scope by hand — becomes a notification with a decision attached.

Wedges become more meaningful. Separating "the machinery is stuck" from "this is waiting on a person" removes a class of misdiagnosis where an operator reads a wedge, grants attempts, and buys laps for a lane that will decline every one of them.

Surface under-scoping becomes measurable. A journaled request per incident turns "surfaces are sometimes too narrow" into a rate, per member size and per crate, which is the input a decision about default surface width actually needs.

The budget exists because the appeal is the new attack surface. A crisp gate with an unbounded appeal teaches a lane that the route past containment is to ask, and a lane that learns to request `crates/**` has defeated containment through the door this decision opens. The literal-paths rule and the required citation are load-bearing, not ceremony.

The amendment path does not bypass sealing; it re-seals. That is what keeps it from becoming a general-purpose scope rewrite: the successor goes through the same door, under the same approval requirement, as the bloom it replaces, and the command that proposes it can only union.

A new approval route reaches the owner. A human-tier amendment interrupts him mid-bloom, which is the correct cost — it is the same interruption a human-tier scope would have caused at seal, arriving later because the need was discovered later.

Follow-on work this creates: an advisory pre-seal check that resolves symbols named in a scope against the declared surface and flags the ones that fall outside it, which would have caught the originating incident; and a measurement over landed history of how often a candidate needed a path its member did not declare.

## Alternatives considered

- **A pre-seal check alone** — cheap, and it would have caught this incident, since the contradiction was written in the scope. It cannot catch the general case, because the lane learns at construct time what nobody knew at seal time. Worth building, insufficient as the only answer.
- **Wider default surfaces** — removes the failure by removing the guarantee. A surface broad enough to never be wrong does not constrain anything, and containment against a declared surface is the property the estate sells.
- **An operator repair past the containment gate** — the fastest recovery, and the one this decision exists to avoid. It lands work outside the surface its approval attests, which trades an hour of operator time for the integrity of every containment claim the estate makes.
- **Always supersede to widen** — this is what the decision does. The objection at the time was cost rather than correctness: a hold, a hand-authored revision, a re-sign, a re-approval, and a supersede. `cargo xtask bloom amend` composes those into one command, so the heavy path became the cheap one and there is nothing left for the repair above to be faster than.
- **Letting the lane widen its own surface and recording it** — the record would be complete and the guarantee would be gone, since the party constrained by the boundary would be the party moving it.
- **Letting the coordinator widen an `auto`-tier delta unattended** — tried, and retracted. The same objection survives the move from the lane to the machinery: the estate would be reading a policy file to authorize itself past a boundary an operator's signature drew, and the policy would be doing the work a signature is supposed to do. The saving was an operator's minute; the cost was that no surface increase carried anybody's name.
