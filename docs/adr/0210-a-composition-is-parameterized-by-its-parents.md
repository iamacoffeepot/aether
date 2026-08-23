# ADR-0210: A composition is parameterized by its parents

- **Status:** Proposed
- **Date:** 2026-08-22

## Context

A bloom's members construct in parallel against one sealed base, so two candidates can each be correct, each pass their own Verify, and still refuse to build in the tree that holds both. On 2026-08-22, bloom `abd504afd855` produced exactly that: one member added a test calling `.contains()` on a value a sibling concurrently collapsed into an `EvidenceChannel` enum. Each verified green alone. The diagnostic named both files:

```
error[E0599]: no method named `contains` found for enum `EvidenceChannel` in the current scope
   --> xtask/src/transform/verify/mod.rs:3048:26
   ::: xtask/src/transform/mod.rs:129:1
```

Neither file belongs to the member that saw the failure. The first member verified on the fold of both was a console member whose declared surface is `crates/aether-bloomery-console/**`, and every lever the reducer holds is member-shaped, so the verdict landed on it. Its Refine lane then declined — correctly, because repairing the failure meant editing a file its approval does not cover — and the member parked awaiting a person. It sat for three hours.

ADR-0191 already gave the woven tree a subject: the composition workpiece, whose candidate is the weave of every member's candidate and which walks the same line members walk. That mechanism works, and it is not what failed. What failed is that it exists at exactly one arity. When the whole-bloom weave refuses over a disagreement between two candidates, the only owners the estate can name are "all of them" — putting a bloom's whole tree under one repair lane for a two-file problem — or "whichever member happened to be verified", which is what it did.

The estate's escape hatch made it worse. A lane's surface request (ADR-0207) is always a literal file path, and the seal door refuses a declared-surface entry naming a file no approval-policy rule names, so the request could not be granted without being rewritten, and `xtask bloom amend` failed three ways trying (#5436). Even granted, the outcome would have been a console member widened into `xtask/**` to repair a disagreement between two other members, which records the wrong thing about who did the work.

A second defect fell out of the same shape. Once the coordinator dispatches a repair lap onto the folded tree, the candidate that lap produces carries every sibling's resolved work in its history. Containment measured that candidate against the bloom's sealed base, charged the member with forty-five files its siblings wrote, and told the repair lane to fix them — which means reverting its siblings' work and re-colliding on the very next fold.

## Decision

**There is one way candidates are merged: a composition over a parent set. The arity is what varies.**

The bloom's fold is the composition whose parents are every live member. A repair of two colliding candidates is the composition whose parents are those two. Same record, same journal facts, same lane shape, same bound rule, same session rule, same inputs. Nothing about a narrower composition is a second mechanism; it is the same mechanism, narrower.

**Narrowing picks the parent set.** When a composition's gate refuses, the coordinator reads the failing diagnostic's paths against what each candidate in that composition changed. If a proper subset accounts for every named path, the composition over exactly that subset is what repairs it. If one candidate accounts for all of them, that is an ordinary finding against that candidate. If the candidate under verification is one of the writers, it keeps its own finding — narrowing may not become a way to launder a defect a member owns. If no subset accounts for the paths, or a named path falls outside the subset's approvals, narrowing refuses: the real owner is found first, and only then does a surface request to a person become the answer.

**A composition's bound is the union of its parents' approved surfaces.** Uniformly, at every arity — so the whole-bloom instance's bound is every live member's surface, and a two-parent instance's is those two. Derived, never signed: every glob in it is a surface one parent's own signed revision already carries, and every parent was admitted under the same bloom-wide approval policy, so the tier ladder was satisfied for every path in the bound before the collision existed. The composition's tier is the strictest parent's, which is what resolving the policy over the union answers directly. Containment against the bound is still containment: a path outside every parent's surface still fails, and still parks.

**A composition's objective is coexistence, and nothing else.** Make its parents' candidates stand on one tree: every intent survives, the workspace compiles, and what each parent passed alone still passes. Author nothing new, touch nothing no parent did. If the intents cannot coexist, report which parent's intent had to give and stop — that is the members' decision, not the composition's.

**A composition runs in a session of its own.** No lineage from any parent, because a parent's session believes its design is right and the others are intruders; that belief is what produced the refusal this decision exists to remove. Its laps resume its own session and its own checkout.

**A composition is given, at every arity:** the base tree and the failing fold; each parent's candidate as a labelled diff against base; each parent's scope document; the gate diagnostic verbatim; the bound as globs; each parent's solo Verify evidence; and the objective above. Nothing from any parent's transcript.

**Its parents are recorded.** The narrowing folds the parents, the diagnostic paths, and the bound onto the journal and renders them on `/view`, because a derived bound would otherwise be unreadable after the fact. "Who caused this, and what was the repair allowed to touch" has an answer without opening a transcript, and the arity is the parent list's length.

**A composition carries no commission and needs no approval.** It takes the same maps a member takes — a stage cursor, a wedge, a dispatch slot — keyed by an id under one namespace: the bare id for the whole-bloom instance, whose parents change as members withdraw and so are read off the bloom, and the id plus a sorted parent list for a narrower one. A second refusal over the same parents lands on the composition already repairing them rather than buying a second lane. It has its own attempt budget and wedges when that budget is spent. One predicate answers "is this a composition" at every arity, so every door that already refuses, filters, or routes the whole-bloom instance picks up a narrower one without a second special case.

**Containment judges a candidate's own delta.** The host stages the whole worktree and commits once, so a lane's output is always exactly one commit on top of whatever it checked out; the commit's first parent is therefore the tree that lane was given — the construct base on a first lap, the previous capture on a repair, the folded head on a lap dispatched onto a composition. That is the range containment measures, falling back to the range the work order names only for a root commit with no first parent, so the gate never quietly stops running. Completeness survives the narrowing because containment runs at every Verify: each lap's delta is judged at that lap's own gate, and the laps compose.

## Relationship to ADR-0191 and ADR-0207

This supersedes the attribution half of both and leaves the rest of each standing.

**ADR-0191** keeps the composition workpiece and everything it says about members being immutable after review, about composition defects repairing in the composition, and about the bloom-level tail retiring into the ordinary line. What changes is that "the composition" becomes "a composition": §5's "composition defects repair in the composition" now reads "a defect in a composed tree repairs in the composition over the candidates that caused it, at whatever arity that is." The whole-bloom instance is unchanged in every respect except that it now states its parents and runs under their union rather than under no bound at all.

**ADR-0207** keeps the surface request, the park, the tier ladder over a delta, the budget, and the literal-paths rule. What it no longer covers is a fold collision. Its worked example — a member declining to edit outside its surface, and the request that follows — is this collision seen from inside one member, and answering it by widening that member's surface records the wrong thing about who did the work. A surface request is now the answer for a path no member of the bloom can reach, not for a path a sibling already holds.

ADR-0207's own unfinished half stands and is required, with one correction: no delta is granted by the machinery, at any tier. Every widening is an operator's decision, made and signed through `cargo xtask bloom amend`. What the estate owes is that the park reach a desk loudly — top-level on `/view` beside the base alert, its own class in the console, and a doctor invariant that fails once the park outlives a bounded age. A stop nobody can see is what turned three minutes of work into three hours.

## Consequences

- The bloom stops stopping for this. A collision that cost a park, an operator, a hand-authored scope revision, a re-sign and a supersede now costs one dispatch against a composition that already knows what it is for.
- Attribution stops lying. The member that happened to verify the fold is not the member that caused the failure, and the journal now says so — which also makes the collision rate measurable per candidate pair, the input a decision about fold ordering actually needs.
- The whole-bloom composition gains a bound it did not have. Its repair lane runs today with no declared-surface containment at all, because containment gates on the member `Verify` stage and resolves its surface from a stored scope revision, and the composition's dispatch carries neither. Under one uniform rule that lane is bounded by the union of its parents' surfaces, which is a tightening.
- A repair lane stops being told to revert its siblings. That was not a cosmetic misreport: acting on it re-collides, so the estate could spend laps oscillating.
- A derived permission is new in the estate and needs its own discipline: the bound is recorded on the journal at the moment it is used and never re-derived from the parents' current surfaces, so a later re-scope cannot silently rewrite the history of a bound already spent.
- Follow-on work this creates: routing a diagnostic path to its owning member in the single-member Verify the way the batch gate already does; and appending the standing shared roots to a glob-declared surface, so a member that declares paths rather than crates can still fix the tooling its own change broke.

## Alternatives considered

- **A second workpiece kind for collisions.** The first shape drafted, and rejected on sight: two ways to merge candidates means two lane shapes, two bound rules, two session rules, and two sets of doors to keep in step, for a difference that is one integer.
- **Widen the verifying member's surface to the union of the whole bloom.** Removes the stall, and is wrong about ownership — a console member ends up authorized for `xtask/**` and recorded as the author of a repair it did not want. The union survives as the *bound*, attached to a composition that is honestly about its parents.
- **Leave every collision at arity N.** Available today and needs no narrowing at all. Rejected because it puts a bloom's whole tree under repair for a disagreement between two files, hands that lane every member's surface, and gives its reviewer a question about the composite when the question is about a pair.
- **Re-open both parents at Refine.** The pre-ADR-0191 behaviour; discards finished reviewed work by construction, and asks each parent to reconcile against the other from inside a session that believes the other is the intruder.
- **Let the surface request answer it.** What the estate tried. The request is a literal path, the seal door admits only globs, the tier ladder then reaches a person for a decision nobody needs to make, and the outcome misattributes the work.
- **Keep containment measuring against the bloom base and exempt sibling paths.** Cheaper than reading the capture's first parent, and it needs a list of which paths are siblings' — the thing a fold makes expensive to know. The first parent is already exactly "the tree the lane was given" and costs one `rev-parse`.
