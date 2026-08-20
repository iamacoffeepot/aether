# ADR-0208: Scoping Is A Dispatched Stage

- **Status:** Proposed
- **Date:** 2026-08-20

## Context

ADR-0149 declared the line as a closed stage vocabulary compiled into Rust — *sketch, scope, approve, construct, verify, refine, review, integrate, aggregate verify/review, land, study* — where each stage binding names the artifact kinds it consumes and produces, the agent profile that runs it, the process it executes, its completion gate, and its retry budget.

Scope is declared and unrun:

```rust
StageId::Scope => (&["bloom.sketch"], &["bloom.scope"], "aether.bloomery.api", "plan-present", 1, 3_600),
```

Every stage downstream of it runs as a dispatched lane against a seat and produces an artifact. Scope does not. A comment at its arm states why:

> Scope is a pre-seal operator-harness process (ADR-0149 §The line, ADR-0150): the operator's own developer-side Bloomery session authors the scope revision and stages it through the REST control API — never a dispatched worker lane.

Neither cited ADR says this. ADR-0149 §The line puts scope in the pipeline on equal footing with every other stage and frames the whole line as moving work "from sketch to merge with little human attention." ADR-0150 concerns per-developer instances and claim-ref coordination and does not discuss scope authorship. The decision was asserted at the implementation site, has been load-bearing since, and was never recorded, reviewed, or argued. It is enforced in code — `Transformation::for_member_stage` carries `StageId::Scope => unreachable!(…)` with a test pinning the panic.

What fills the gap is an operator writing scope documents by hand. That has a measurable failure rate. On 2026-08-19 a member's scope declared `crates/aether-bloomery/src/**` while naming its own work three times as happening at seal admission, in a different crate; the contradiction was visible in the document before anything ran, and cost a wedge, a hold, a hand-authored revision, a re-sign, and a supersede. On 2026-08-20 two members of one wave were refused by containment on the same class of error, from scopes written in the same session: one omitted the re-export its own new type required, and one changed a port trait method without declaring the crates holding that trait's three implementors — after the authoring session had already run the search that listed them.

The estate had already found this and priced it. Under ADR-0146, `land`, `approve`, and the approve sweep pinned reasoning effort to `low` and `implement` pinned `medium`, while scope was left at the maximum on both axes:

> scope's default **INVERTS to opus** (#3203): a pre-scope issue cannot carry the label — scope is what stamps it — so the label read is structurally a no-op for fresh scope work, and the sonnet default was **guaranteeing the ladder's most judgment-heavy task the cheapest model.**

`StageCatalog::profile_of` still carries that verdict today: `StageId::Scope` resolves to `(Harness::Claude, OPUS_MODEL, High)`. The seat is declared. Nothing dispatches against it.

### What the corpus shows

A survey of all 48 hand-written scope documents and the 78 stored revisions behind them establishes three things.

**The failure rate is real and small, and it cannot be measured properly.** Containment refusals are not durably recorded: the findings text is never journaled, `apply_containment` stamps `VerifyFailure::Test` so a containment refusal is indistinguishable from a test failure, and the only text record is a mutable projection overwritten per attempt. Seven members are observable as having produced one — 12.3% of workpieces that reached Verify — with the true figure somewhere in `[7, 52]` of 187 verify failures and no archive able to narrow it. The durable proxy agrees: `scope_revisions` is append-only, and 6 of 48 scopes (12.5%) had their declared surface widened by a later revision, which is a permanent record that the original was wrong.

**Most of that signal is a different defect.** Of 25 offending paths, 13 are covered by a *sibling's* declared surface — the member blamed for its neighbours' files because a member is diffed against the bloom's base rather than its own lineage. That is the defect this ADR's sibling work fixes, and it means raw containment counts must not be used to calibrate a scoping lane.

**What remains is mechanically derivable.** Eight genuine own-scope misses across three members: five cross-crate ripple, the rest test-directory and re-export ripple. One document of the 48 named a rerunnable search — *"Confirm the implementor set with `git grep adopt_candidate` before changing the signature"* — and then declared a surface covering none of the implementors. Naming a search in prose does not run it.

**Four fields have no consumer.** `**Routing reason:**` is required by the parser, validated, then dropped — the string appears in 0 of 78 stored revisions. `size` is read only by the work-order renderer. `dogfood_brief` has no lane and no gate and is empty in 48 of 48 documents. `implements` is hardcoded to an empty vector with no reader. `**Implementation model:**` reads `grok-4.6` in 48 of 48 and is consumed as a presence bit, the actual seat coming from a separate sealed override.

Success means a workpiece is filled by a dispatched lane against a named seat, verified before it is frozen, and reproducible from journaled state.

## Decision

### What does not change

Approval is a signature over a frozen workpiece, attesting it exactly as it reads at freeze time. An unapproved workpiece does not qualify to enter a bloom. Sealing admits approved workpieces and nothing else.

Every modification voids what preceded it. A workpiece may be forked and edited freely — by a lane, or by an operator by hand — and each edit produces a new frozen version that must be signed afresh. The estate already enforces this: `admit_loaded` refuses a seal naming any revision that is not the commission's current tip (`StaleScope`), and the approval statement's words *are* the scope digest. Tier follows from the aggregate of what the workpiece touches, resolved over its declared surface by the existing ladder.

None of that is in question here, and none of it moves.

### What changes

**Only who fills the fields before the freeze.** Today that is an operator writing markdown. It becomes a dispatched lane writing typed fields, verified before freezing. Scoping runs before the freeze, therefore before approval, therefore before sealing — so the seal's admission door is untouched by this decision.

**A workpiece is a set of typed field records, not a document and not one struct.** This is the load-bearing shape decision, driven by an existing constraint: content-addressed values encode positionally and untagged, so a persisted struct is frozen at its digest forever, and adding a field changes what old bytes decode to. One large struct is the least evolvable shape available, and a scoping vocabulary that cannot gain a field will be wrong permanently. `ScopeRevision` is that struct today, and the estate has already broken its own rule about it — the `implements` field was appended, the golden bytes repinned, and `schema` left at `1`, which survived only because nothing signed predated it.

Enums append safely, and the estate carries the idiom in `Evidence { subject, kind, detail }`:

```rust
pub struct WorkpieceFact { workpiece: WorkpieceId, kind: FieldKind, detail: Digest }

pub enum FieldKind {
    Problem, Evidence, Success,
    Approach, RejectedOption, AffectedSurface,
    PlanStep, Acceptance, DeclaredSurface,
    InverseSearch, Edge, RoutingHint,
    // append only — never reorder, never remove a variant
}
```

Adding a field is appending a variant. Removing one is ceasing to produce it; every persisted workpiece still decodes. A consumer wanting a field an older version does not carry reports it **absent**, distinct from present-and-empty.

**A field with no consumer is not carried forward.** The survey found four, and the vocabulary drops each rather than reproducing it: the routing reason (required, then discarded, and absent from every stored revision), the authored size (no gate or router reads it, and it is derivable after the fact from the candidate diff), the dogfood brief (no lane, no gate, empty in every document), and the affected-surfaces prose (a second unvalidated statement of the declared surface — two descriptions of one thing drift, and the one with a gate behind it wins). The ADR reference the dead `implements` field was meant to carry becomes a real field, since it has a resolvable target. Nothing here proposes building a dogfood lane; the brief is dropped because nothing consumes it, and it returns only if a consumer exists first.

**Emission is incremental and validated per field.** A lane fills a workpiece through a builder surface where each setter is last-write-wins, so a lane that re-emits a field overwrites rather than duplicating. Three reasons this beats one structured blob: the load-bearing invariants are cross-field and need a terminal check that refuses with the specific violation rather than a schema mismatch; validation arrives when a field is written rather than after the whole artifact is committed; and, decisively, the builder **fills the derived fields itself** — when a plan step names a symbol, the inverse-dependency search runs and its results are stored, so no lane is asked to author that field and none can skip it.

**The workpiece declares its edges.** A dependency between units of work is true before any wave exists, and today that knowledge lives only in an operator's head. The seal resolves each declared edge three ways: the dependency is in this wave (a declared edge gating dispatch, as today), it has already landed (satisfied), or it is neither — which is a refusal naming an unsatisfied prerequisite rather than today's `UnknownWorkpiece` error. Surface-derived edges are unaffected and keep ordering integration only.

**Routing is a hint, not a pin.** A frozen artifact must not carry a model name; seats change, and this ADR changes one. The workpiece declares properties of the work — size, remaining judgement, risk class — and the profile catalog maps those to a seat at dispatch.

**The workpiece is verified before it is frozen.** A candidate can be wrong in ways only execution reveals; a workpiece can be wrong in ways only search reveals, and that search is mechanical. Resolve the symbols the plan names against the symbol inventory — `xtask/src/symbols/` already builds it, and every row carries its defining path — and report any whose file no glob in the declared surface admits, reusing `containment::path_in_surface` rather than a second matcher.

**Widening is reported, never silent.** `resolve_member_dependencies` derives a dependency edge between any two members whose declared surfaces intersect, so a builder that quietly widens a surface manufactures splice edges — more serialization, and more of the lineage contamination described above. A derived field that would widen the surface reports the sibling intersections it creates and leaves the widening to the author. Auto-completion that hides a scheduling consequence is worse than the omission it fixes.

One check refuses rather than reports: **the declared surface must cover the paths the workpiece's own plan steps and inverse searches name.** That is internal consistency of the artifact with itself, not the reverse-dependency closure, and it is decidable. #5256 and both 2026-08-20 refusals were violations of it. Everything else reports three outcomes — resolved inside, resolved outside, unresolvable — and never refuses, because requiring a surface to cover every symbol its prose mentions inflates surfaces until containment constrains nothing.

**A gate is a verdict, not a name.** No `completion_gate` string is read anywhere in the workspace — `pr-open` and `ci-green` included; the field is inert vocabulary. The mechanism is `verdict_passed` over a `StageVerdict` at intake. The scope stage's gate is therefore a lane emitting a verdict, an intake arm routing it, and a retry budget.

**The seat is `grok-4.6` at high effort, recorded as a cost-bound deferral.** ADR-0146's finding that scoping is the most judgement-heavy task on the ladder is not retracted, and `profile_of` still carries `OPUS_MODEL` for this stage. It is overridden on cost, deliberately and on the record, so a future session reads a decision rather than an accident. The seat is a stage-binding value and moves without an ADR.

**The retired `/scope` skill is not adopted as the specification.** It is the estate's least legible process, its obligations are style-shaped rather than machine-checkable, and preserving its section layout would preserve the variation this decision removes. It is read as raw material — an inventory of what a scoper must establish — and each item survives as a typed field with a stated validator, or not at all.

**The manual path stays, and is visibly manual.** An operator may fork a workpiece and edit it by hand; recovery must not depend on the lane being repaired. Such a version carries no scope-verify evidence, and its absence is reported rather than treated as a pass.

### The one genuinely missing piece

Every dispatch today originates from the reducer acting on a bloom. Scoping runs before a workpiece qualifies for a bloom, so it needs a producer for `Topic::Dispatch` that is not the bloom reducer, plus journaled state tracking a scoping run. The executor drains that topic and does not care who enqueued. That seam is where this decision lands, and it is the only new dispatch machinery required.

## Consequences

Scoping becomes measurable. Today its quality is an anecdote about how thorough an operator was; afterwards each workpiece carries a verify verdict, and "how often does a member touch a path its workpiece did not name" is derivable from journaled state.

The operator stops being the throughput bound on the step that gates every later step, and stops being its sole quality control. Workpieces become uniform, so a construct lane receives the same structure every time instead of prose reconstructed from parsed headings.

This does not retire ADR-0207. Scoping predicts work that has not been done. A verified lane removes the systematic half of that error — the unsearched trait, the undeclared re-export — while roughly half of what a member ends up touching is not derivable from its workpiece at all. That residue still needs the surface-request path, and uniform workpieces make it cheaper to build, because what a member asked for that its workpiece did not name becomes measurable.

Reads get more expensive. A workpiece is N records to gather and project rather than one row to decode, and every consumer must handle an absent record rather than a field that is always present. That is the right trade — absent is real information a struct cannot express — but reads are where the estate spends its time.

The record shape generalizes past scoping to any journaled artifact facing the same evolution problem. That is deliberately out of scope here; it follows as its own decision once this pattern has run once.

## Alternatives considered

- **Run scope as a member stage inside the bloom.** Rejected: a member is sealed *with* its frozen workpiece — the entry dispatch's subject is that digest — so a member cannot enter at a stage whose job is to produce it. Escaping that requires moving approval in-line, which fixes a bloom's identity before its authorization and turns a pre-flight refusal for owner-tier work into a mid-flight stall.

- **Build the pre-seal symbol check and keep hand-authored workpieces.** The original plan for #5291, and insufficient: the check is advisory, so it produces a report the same operator who wrote the flawed surface must read and act on. It removes no variance from the authoring step. Relocated here, it gains a producer whose output it can gate.

- **Keep scoping in the operator harness and improve the discipline.** The obligations that would have prevented both 2026-08-20 failures were already written in the `/scope` skill and were not followed. A process depending on an operator remembering a checklist at the end of a long session has a failure rate no restatement changes.

- **Let the construct lane scope itself.** Collapses the prediction into the work, which removes the boundary rather than the error: a lane that writes its own surface is not contained by it, and the approval attests nothing.

- **Make the whole scope verify refusing.** Rejected except for the self-consistency check: it drives surfaces toward the reverse-dependency closure and empties containment of meaning.
