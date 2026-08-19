# ADR-0202: The composition of workpieces is itself a workpiece

- **Status:** Proposed
- **Date:** 2026-08-18

## Context

A bloom's members walk one line: Construct produces a candidate, Verify proves it mechanically, Review judges it, and a member that passes is resolved. The bloom's integration tail is built differently. Integrate folds the member candidates; a fold collision re-enters the *member* at Reconcile with an advisory overlay; the composed tree then passes through aggregate verify and aggregate review, and an aggregate refusal can dispatch fresh `construct.implement` orders against members that already passed their own review.

Three incidents mark the cost of that asymmetry:

- Bloom `05b1f598` (2026-08-14): an aggregate refusal re-entered members and discarded four finished, reviewed candidates. The defect was in the composition; the price was paid by the members.
- Bloom `10a1228c`: aggregate review ran as a full re-read of every member diff and spent three judge rounds drifting across work that member reviews had already judged.
- Bloom `227270a9` (2026-08-18): two members whose diffs collided on shared wiring were re-entered at Reconcile concurrently, each blind to the other's re-entry against the same fold. Both passed verify; integrate folded one and wedged both. The race exists only because seam repair is dispatched *per member* — the collision is a property of the composition, but no single workpiece owns it. (#5163 serializes those dispatches as a bridge fix inside the current model.)

Separately, the tail's special-case machinery is where transport pathologies have concentrated: the Reconcile overlay carries a member's whole conflicted candidate as advisory text, which is how a composed task reached 235 KB and tripped the `execve` argv limit (#5161).

The underlying force: the pipeline has exactly one theory of work — a workpiece with a work order walks Construct, Verify, Review, with its own findings channel and retry budget — and the integration tail is the one place that theory is not applied.

## Decision

When every member of a bloom resolves, the coordinator creates one synthetic workpiece, the **weave**, and it walks the same line as any member:

- **Construct** is the composition itself: fold the member candidates in dependency order; where candidates collide, author **seam edits** — the minimal edits that let both members' changes coexist. The weave's candidate is the fold plus its seam edits. A clean fold is a valid, nearly free construct; the weave workpiece exists for every bloom, not only for blooms that collide, so the board shows one ontology everywhere.
- **Verify** is the mechanical gate run over the composed tree — the work aggregate verify performs today, unchanged in substance, now owned by an ordinary workpiece.
- **Review** judges the weaving, with a charter of exactly two questions: did each member's intent survive the composition (the seam edits preserved what the member set out to do), and do the member intents interact correctly in the composed tree (including interactions that produced no textual collision at all). The member work orders are the review's reference input; the weave is its subject. Review does not re-read member work — that judgment was already rendered, once, by the member's own review.
- The weave has its **own findings channel, refine loop, and retry budget**. A weave defect — a wrong seam edit, a fold ordering mistake — repairs in the weave: fix the seam, re-fold, re-verify. It never dispatches against a member.

**Members are immutable after review.** A member workpiece that has passed its review is done; no stage may dispatch against it and no bloom-level outcome may re-enter it. A weave-review finding that is genuinely about member code does not reopen the member — it is recorded and filed as a new issue for a future bloom, the same way the team fixes `main` forward rather than reopening merged pull requests. Contested cases resolve through the manager override (#4957) with a recorded reason.

**The escape for member-owned failure at weave time is ejection.** When the composed tree fails verify for a reason the weave cannot own — the member's code is wrong in composition, and no seam edit within the weave's charter repairs it — the bloom supersedes ejecting that member: the survivors' candidates re-prove on the reduced composition and the ejected issue rides a future wave. Ejection is the only transition that removes a reviewed member from a bloom, it is explicit and journaled, and it costs the ejected member's future lane time — never the survivors'. This transition must exist in the machinery, not as an operator patching a worktree; the alternative is that "member-owned red at weave time" becomes a wedge class with a new name.

The economics rule that generalizes all of the above: **a defect never costs more than the subject it belongs to.** A seam defect costs weave lane time. A member defect discovered late costs that member's ejection. Nothing discards finished, reviewed work that was not itself defective.

### What this abolishes

- The refusal re-entry path: an aggregate outcome dispatching member `construct.implement` orders. Under this model the transition does not exist.
- Member-dispatched Reconcile and its fold-conflict overlay: seam authoring happens in the weave's construct, where every collision is visible in one place, so concurrent blind re-entries against the same fold cannot arise (#5163 becomes unnecessary and is removed with this path). Seam tasks carry colliding hunks and the owning intents, not whole candidates, which also retires the overlay's pathological task sizes.
- Aggregate verify and aggregate review as special-cased stages: `Integrate`/`AggregateVerify`/`AggregateReview` become the weave's ordinary `Construct`/`Verify`/`Review`.

### Identity and work order

The weave is addressed like any workpiece, named by its bloom (`weave-<bloom-digest-prefix>`), and carries a derived work order: the composition of its members' scope revisions. With the commission store (ADR-0199 slice 2) landed, that derivation is structural — the weave's review consumes the members' canonical scope revisions as reference input rather than scraped issue prose. This ADR therefore lands after the commission store slices.

### Wire and configuration

No new `StageId` variants: the weave walks the existing `Construct`/`Verify`/`Review` stages. The decisions graph is extended append-only with a workpiece-kind discriminator (member or weave); historical journals replay unchanged, blooms recorded under the old tail replay through the retained variants forever, and the golden decisions fixture widens with a pure append in the same diff. The existing `AggregateReview` seat key in model overrides remains valid and now names the weave's Review seat, so seat configuration does not move.

## Consequences

- One ontology: the board, the operator ladder, findings channels, retry budgets, and seat routing describe every workpiece the same way. Special-case tail machinery (refusal re-entry, fold-conflict overlays, reconcile serialization) is deleted rather than maintained.
- Reviewed member work is never discarded by a composition-level outcome; the `05b1f598` class is structurally closed.
- Weave review is sharply scoped, which trades a full second read of member code for an explicit charter. The interaction question in the charter is what covers cross-member defects that produce no textual collision; if that question is dropped from the review prompt, this model has strictly less coverage than a diligent full re-read — the charter is load-bearing, not advisory.
- Ejection becomes ordinary machinery with a journaled transition, replacing the operator supersede-with-patched-xtask recipe.
- Follow-on work: the reducer/stage-catalog change modeling the weave workpiece; the weave construct transform (fold plus seam authoring) and its task shape; retiring the Reconcile dispatch path and #5163's serialization once the weave path carries collisions; console rendering of the weave as a member-like row.

## Alternatives considered

- **Keep the current tail and harden it case by case** (serialize reconciles, cap overlay sizes, forbid refusal re-entry by policy): rejected — each fix patches one symptom of the same asymmetry, and the incident list above is that asymmetry generating failures faster than patches land.
- **Materialize the weave only when candidates collide**: rejected — it is leaner per clean bloom, but it reintroduces exactly the special case this ADR abolishes, and a clean fold's construct is nearly free.
- **Allow the weave's refine loop to edit member code under a size threshold**: rejected — a threshold turns scope creep into a tunable, and the immutability rule stops meaning anything at exactly the moment it matters (a large member defect discovered late).
- **Aggregate review as full re-read with a better prompt**: rejected — `10a1228c` shows the drift is structural: the reviewer's subject was every member's work at once, which no prompt scopes down as hard as making the weave itself the subject.
