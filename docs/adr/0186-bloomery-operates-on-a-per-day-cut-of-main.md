# ADR-0186: Bloomery Operates on a Per-Day Cut of Main

- **Status:** Accepted
- **Date:** 2026-08-13

## Context

Bloomery and humans share one integration point: `MAINLINE_REF` is hardcoded to main, so any out-of-band merge — an ADR, a hotfix, an operator landing a pull request — moves mainline out from under every sealed bloom and forces supersession (`land_proposal` compares the live ref against the sealed base and returns `BaseMoved`). A documentation merge mid-flight has already doomed a two-member bloom's landing and discarded its construct lanes' work. The bloom train (ADR-0185) removes the landing barrier *between blooms*; nothing removes interference from everything that is not bloomery. Isolation is the third leg of the throughput arc alongside pipelining (ADR-0185) and width (the lane ceiling): with one writer on bloomery's mainline, train projections essentially always hold.

The economics are coupled to the same synchrony. Paid runners exist to make synchronous per-pull-request proof fast, and the in-pipeline verify gates run the full workspace on every lap because each landing must be fully proven the moment it merges. Both costs follow from proving every landing at landing time rather than proving the day once.

## Decision

Bloomery operates on a branch per day, cut from main.

- **The ref.** Each operating day starts by cutting `bloomery/daily/<date>` from current main. The coordinator's mainline points at that ref: blooms seal, land, and train against it exclusively, and nothing else writes to it. The morning cut is the main→bloomery sync point, so divergence from main is structurally bounded at one day with no separate resync ceremony. The daily ref carries no required checks of its own; bloomery's gates prove each landing.
- **The repoint.** The mainline ref is boot-resolved configuration (the ADR-0090 derive-`Config` path), not a sealed value: a sealed base already pins each bloom to the exact commit it builds on, so sealing the ref name would only freeze the roll. Today the roll repoints by restarting the coordinator (observation is boot-only); when observation becomes a polled, admitted fact, the repoint becomes one more observation and the restart disappears. Either way the journal records which head was observed, so replay is unambiguous.
- **The sync-back.** The day's landed blooms return to main as one integration pull request, gated by the full required CI once and merged by **rebase-merge**, so each bloom's squash commit — model-authored subject and `Closes` lines intact — lands on main verbatim. The rebase-merge carve-out applies to the sync pull request only; human flow stays squash-as-today. The invariant is "main receives only fully-proven trees", not "every landing is fully proven".
- **The day roll is a quiesce point.** The coordinator has one mainline pointer, so day branches never overlap: stop sealing, drain the train, open and merge the sync-back, cut tomorrow's branch, repoint, resume. The trigger is calendar-daily; an undrained bloom drains on its branch and the roll waits — no bloom is orphaned by the calendar. A day whose sync-back CI fails outlives its date: the branch stays bloomery's mainline until a repair bloom fixes it and the sync merges, and the next cut is always taken from post-sync main. Claims and the admission ref are repository-global and empty at the roll by construction. The roll is mechanical: an operator command first, coordinator-owned later.
- **In-day CI relaxes by what a rerun can restore.** Inside the day the branch is an optimistic integration zone. Correctness gates (test, clippy, docs — the expensive ones) protect the tree, which the day-end full run backstops: they may scope to the diff's reverse-dependency closure, with the fail-open list covering workspace-level inputs and component crates (a chassis test that loads built wasm depends on the component crate through the filesystem, not linkage). Integrity gates (suppress, declared-surface containment, approval binding — all seconds) protect trust, which does not batch: a day of stacked suppressions found at sync-back is an unwind, not a bisect. They stay strict per-bloom. The rule: be lazy about what a rerun can prove, never about what a rerun cannot restore.
- **The backstop.** The full suite runs eagerly but asynchronously on a standard GitHub-hosted runner at each daily-branch landing, non-blocking. A red result quarantines the branch and triggers a repair bloom, capping a scoping miss at about one bloom instead of one day; a miss is bisectable by construction (one authored commit per bloom) and each is a countable ledger event marking a closure blind spot. Lazy-at-sync-back is the zero-cost fallback if runner spend matters more than blast radius. Paid runners are retired from the bloomery path: eve carries the in-day hot loop, one standard runner notarizes the day.
- **The playground is a sibling, not a tenant.** Exploratory agent work gets `sandbox/*` refs cut from the day's branch — same fresh tree, zero write access to bloomery's ref. Anything worth keeping graduates by becoming a work order bloomery executes with proofs. Free play on the bloomery ref itself would reintroduce exactly the `BaseMoved` class this removes.

## Consequences

- A sealed bloom can no longer be doomed by anyone who is not bloomery; supersession returns to meaning what it says — a real conflict inside the pipeline.
- Issue closure moves to the day boundary: a bloom's `Closes` lines fire when its commit reaches the default branch at sync-back, not when its landing proposal merges.
- The per-bloom floor on the daily ref approaches construct time plus about ten minutes once verify scopes to the closure and identical trees stop being re-proven; both of those become safe *because* the day-end full run backstops them.
- The branch-per-day scheme doubles as a record: each day's work is one ref, one sync pull request, one full-proof receipt.
- Follow-on slices: the mainline-ref configuration knob, the roll ceremony (operator command, then coordinator-owned), the sync-back automation, the async backstop workflow and its quarantine wiring, and retiring the paid-runner path.

## Alternatives considered

- **Keep sharing main, coordinate socially.** Proven insufficient live; one forgotten mid-flight merge burns lanes.
- **One long-lived bloomery branch with periodic resync.** Divergence is unbounded and the resync becomes a merge project; the per-day cut bounds it structurally and gives records for free.
- **Granularities larger than a day.** More divergence, larger sync-back blast radius, worse records; the day is the natural unit.
- **Squash the sync-back.** Flattens the day into one blob and undoes the model-authored per-bloom subjects and closing lines.
- **Full synchronous CI per landing (status quo economics).** The paid runners exist only to make that synchrony bearable; removing the synchrony is cheaper than accelerating it.
