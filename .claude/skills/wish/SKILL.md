---
name: wish
description: Adversity-grounded design ideation that drills from a felt absence down through the chain of "I don't know how to produce this yet" until each branch resolves into a producible plan. Each wish carries its alternatives + doors-opened + doors-closed so integration questions are answerable without re-deriving the design space. Output is a directory tree (one wish per file, alternatives as sibling subdirectories), depth unbounded, no header structure imposed. Wishes assume infinite time, finite compute. Shapes are grounded in current code — every existing surface a wish builds on is grep-confirmed against the crates, not recalled from training, so producibility is real rather than hallucinated. The accuracy test is the same rails as normal execution — the collapsed plan must compile and pass CI.
---

# /wish — recursive design through wishing

A wish is a marker for *"I don't know how to produce this with known means yet."* The skill walks forward from an adversity, articulates the shape of what would satisfy it at the appropriate level of depth, asks whether that shape is producible, and — if not — surfaces the absences as deeper wishes. Recurse.

A wish dissolves into a plan the moment its shape becomes producible by known means within current resources. The leaves of the tree are plans. The interior nodes are wishes whose plans depend on their children landing first. The whole tree collapses upward — child plans satisfy parent wishes, which become plans, which satisfy grandparent wishes, until the root wish is itself a plan.

**The accuracy test is operational.** The collapsed-upward plan must produce code that compiles and passes CI. Same rails as `/scope` and `/implement` already enforce. Wishing is just structured drilling toward those rails.

Each wish carries its design-space context — the alternatives it was picked from and the doors its choice opens or closes — so that integration thinking ("what does this unlock? what does it foreclose? did we pick the path with best forward optionality?") doesn't require re-deriving the analysis later.

## What `/wish` does and doesn't do

`/wish` proposes design through structured drilling. It writes a directory tree of wish files. It does **not** file issues, write production code, or update the project board. The user reads the tree, decides which leaves to file as issues for `/scope` to pick up.

Distinct from `/scope` (formalizing one chosen wish/issue into Define→Design→Plan). `/wish` is the upstream that surfaces what's *worth* scoping.

## Core principles

### Wishes come from adversity

Two sources:

- **Lived data** — capture-log "I wish" hits, repeated friction in transcripts, "fixed this annoying thing" commits, memory entries about pain.
- **Modeled empathy** — well-grounded prediction of friction a role would experience, articulated specifically enough to be falsifiable (which user, in what scenario, why this particular friction predictably emerges).

Wishes that can't name their adversity source — either a specific data ref or an articulated empathy chain — are imagination, not design. Drop them.

### Wishes ground their shape in real code

A wish's *adversity* says why it exists; its *shape* says what would satisfy it. Adversity is grounded by a data or empathy source (above). The shape must be grounded too — but in the **current code**, not the agent's trained recall of it.

The failure mode: a wish composes its plan from kinds, caps, mailboxes, traits, file paths, or API signatures that don't exist or are misnamed. Producibility *looks* satisfied, but the "known means" were hallucinated, so the collapsed-upward plan can't compile. The operational accuracy-test (compiles + passes CI) catches this only *after* implementation — too late. Ground at ideation time instead.

The rule: **every concrete engine surface a wish claims already exists must be grep-confirmed against current code before it is written into the wish.** Read the file; cite `crate/path` (or `file:line`) for load-bearing references. Prefer code over `CLAUDE.md` and ADRs — docs drift, the crates are authoritative.

The line between checked and invented is sharp: the *existing* pieces a wish builds on are verified facts; the *novel* thing being wished for is the design — invented, that's the point of a wish. "Build a new `X` on top of `aether.fs.read` (verified: `crates/aether-capabilities/src/fs.rs`)" — `aether.fs.read` is grep-confirmed, `X` is the wish. If you can't verify a surface you were about to lean on, that's signal, not a detail: either it doesn't exist (a deeper absence — drill it) or it's named differently (find the real name). Never paper over with a plausible guess. This is the [[verify_names_against_current_code]] discipline applied at design time.

### Wishes drill toward producibility

At each wish, the agent sketches the shape that would satisfy the wish *at the depth currently occupied* — coarse near the root (system seams, crate-level architecture, ADR revisions), fine deeper down (traits, file formats, mail kinds), finest at leaves (structs, fields, algorithms, pseudocode).

Then asks: **can I produce this shape with known means within current resources?**

- **Yes** → the wish is actually a plan. The wish file is the plan. The chain ends here.
- **No** → articulate the absences. Each absence is a sub-wish, written as its own file in a sub-directory. Recurse on each.

The bottom of the tree is whenever an articulation reaches "I can write this." Don't pad and don't truncate.

**"Known means" are *verified* means** (see *Wishes ground their shape in real code* above): an existing surface the shape composes from counts as a known mean only once it's grep-confirmed in current code. An unverified surface is itself an absence — drill it or find its real name, don't assume it.

### Plans collapse upward

When all of a wish's children become plans, the wish itself becomes a plan: its body's "given these sub-wishes, the shape composes like X, Y, Z" is the plan. This propagates upward. The root wish's plan IS its tree.

Reading any node at any depth gives a coherent plan-or-wish-with-clear-decomposition. Reading the root tells you the system-level shape. Reading a leaf tells you the algorithm shape.

### Infinite time, finite compute

Wishes assume unbounded patience for design, sequencing, and articulation. They do **not** assume infinite resources. Aether's current resources: one engineer + Claude, modest API budget, no GPU cluster.

- Wishes that require training custom models, running 100 parallel agents continuously, or buying enterprise compute → flag as resource-bound or drop.
- Wishes that require clever caching, batching, incremental computation, or careful sequencing → fine, this is what infinite time buys.

Resource limits aren't a separate filter — they're part of the producibility check.

### Shape comes out of the work, not before it

LOD-appropriate shape — coarse near the root, fine near leaves — is a *consequence* of where in the tree you are, not a schema imposed from outside. Don't pre-bake "level 2 = traits"; let the depth produce the LOD naturally.

### Wishes carry alternatives and doors

Each wish file articulates three additional dimensions beyond shape + plan:

- **Alternatives considered** — every shape worth thinking about for the same adversity, named in the prose body. Why this one over those is answered with **path-cost analysis**, not just shape-rejection.
- **Doors opened** — what downstream features, wishes, or patterns this choice unlocks or accelerates.
- **Doors closed** — what design space this commits to, and what it forecloses. The things you'd have to undo if you changed your mind.

This is ADR-flavored thinking applied at wish granularity. Not every wish becomes a full ADR; but every wish carries enough decision context that an integrator reading it can challenge the choice without re-deriving the design space.

**Regret-avoidance** is the discipline: don't pick the locally-cheapest path if it forecloses a future direction you want; don't pick the locally-attractive shape if its build + maintenance + reversibility cost is worse than an alternative's. The path-cost lens is how to spot regret-bait before committing.

## Format

### Directory tree

```
wishes/<YYYY-MM-DD>-<theme-slug>/
├── index.md
├── <root-wish-slug>/
│   ├── wish.md
│   ├── alternatives/
│   │   ├── <alt-1-slug>/wish.md          ← shallow by default (depth 1)
│   │   ├── <alt-2-slug>/wish.md
│   │   └── <alt-3-slug>/wish.md
│   ├── <sub-wish-slug>/
│   │   ├── wish.md
│   │   ├── alternatives/                  ← every wish can have alternatives
│   │   │   └── ...
│   │   └── <sub-sub-wish-slug>/
│   │       └── wish.md
│   └── ...
└── <another-root>/
    └── ...
```

One `wish.md` per node. Directory nesting encodes wish nesting. Slugs are lowercase kebab-case, descriptive, 20-50 chars. Depth is unbounded.

`alternatives/` is a sibling subdirectory under any wish that names alternatives in its body. The folder may be empty initially (alternatives listed in prose but not materialized as files) or contain shallow alternative wish files (one per named alternative). Alternatives are materialized as files on `/wish --compare <wish-path>`.

`index.md` at the top of the pass holds: date, theme, role, sources scanned, the list of root wishes with one-line summaries, the considered-and-dropped list, and notes. It's the navigation surface, not a duplicate of content.

### Realistic shape — anti-shallow-bias example

The abstract tree above shows depth 3 with placeholders. Real trees vary in depth per branch — some chains bottom out at depth 2, others spill 6+ deep. The shape emerges from the chain, not from a target. Here is a concrete worked example demonstrating that variance, so agents reading this skill don't anchor on the shallow placeholder layout:

```
wishes/2026-05-19-persistent-multiplayer/
├── index.md
├── persistent-world/
│   ├── wish.md                                      ← system-level vision
│   ├── alternatives/
│   │   ├── eventually-consistent-shards/wish.md
│   │   ├── single-authoritative-server/wish.md
│   │   └── peer-mesh-no-server/wish.md
│   ├── player-state-persistence/
│   │   ├── wish.md                                  ← capability-level
│   │   ├── alternatives/
│   │   │   ├── per-component-disk-write/wish.md
│   │   │   └── periodic-snapshot/wish.md
│   │   ├── content-addressed-serialization/
│   │   │   ├── wish.md                              ← mechanism-level
│   │   │   ├── alternatives/
│   │   │   │   └── time-keyed-serialization/wish.md
│   │   │   ├── component-state-trait/
│   │   │   │   ├── wish.md                          ← trait-level
│   │   │   │   └── alternatives/
│   │   │   │       └── runtime-reflection-based/wish.md
│   │   │   └── on-disk-layout-for-handles/
│   │   │       └── wish.md                          ← struct-level (leaf-plan)
│   │   └── restore-on-reconnect/
│   │       ├── wish.md                              ← interface-level
│   │       └── correlation-id-for-resume/
│   │           └── wish.md                          ← leaf-plan
│   └── authoritative-simulation/
│       ├── wish.md
│       └── tick-rate-governor/
│           └── wish.md                              ← leaf-plan
└── concurrent-many-players/
    ├── wish.md                                      ← shallow chain
    └── per-actor-substrate-load-shedding/
        └── wish.md                                  ← leaf-plan (depth 2 total)
```

Discipline checks visible in this example:

- **Depth is non-uniform.** `persistent-world` spills six levels deep through state-persistence. `concurrent-many-players` bottoms out at depth 2. Both are honest. Don't pad shallow chains; don't truncate deep ones.
- **Alternatives can themselves have alternatives.** `component-state-trait` has a competing `runtime-reflection-based` shape; that competitor stays shallow until someone drills it via `/wish --under`. Alternatives nest recursively the same way wishes do.
- **LOD coarsens upward naturally.** `persistent-world` is crate-and-system level; six levels down, `on-disk-layout-for-handles` is struct fields and byte layout. LOD emerges from depth, not from a fixed schema.
- **Bottom-out is producibility, not a target depth.** Some branches reach "I can write this" at the first level (`tick-rate-governor` is a single mechanism). Others descend through capability → mechanism → trait → struct. Both are valid; the chain stops when the chain stops.
- **The shallow chain is just as valid as the deep one.** Not every root wish needs a deep tree. Some have simpler answers; that's honest, not lazy. The skeptical filter is *"did this chain reach producibility?"*, not *"did this chain match my prior tree's depth?"*.

If you find yourself padding a chain to match a sibling's depth, you've drifted. The chain stops when producibility says so.

### `wish.md` shape (chosen path)

Minimal frontmatter, then free-form prose. No internal `##` headers — the prose flows.

```markdown
---
wish: I wish I could <X> so that I could <Y>.
adversity: data | empathy
parent: ../wish.md                # omit if root
supports:                           # optional, only if branch-overlap with memory
  - "[[memory-entry-name]]"
producible: true | false            # true means this wish IS a plan
---

<free-form prose body — no fixed section headers — articulating naturally:>

<- the wish, the adversity that grounds it, the goal it serves>
<- the shape that would satisfy it at this level of depth>
<- whether that shape is producible with known means within current resources>
<- if producible: the plan (concrete enough that someone could start)>
<- if not: the absences, each named with the sub-wish that would resolve it>
<- coherence with the parent: how this wish's resolution composes upward>

<then, in prose, the design-space context:>
<- alternatives considered, named with one-line shape + one-line path cost each. (Materialized as sibling alternative files when /wish --compare is invoked.)>
<- doors opened: what this unlocks downstream — sibling wishes, future features, pattern templates>
<- doors closed: what this commits to / forecloses — paths we'd have to undo, contracts that become public>
```

### `alternatives/<slug>/wish.md` shape

An alternative wish is shallow by default — depth 1, no children. It articulates a counter-path for the same adversity that the parent wish addresses. Frontmatter:

```markdown
---
wish: I wish I could <X via alternative shape> so that I could <Y>.
parent: ../../wish.md
alternative_to: <parent-wish-slug>
producible: yes (shape; shallow — drill with /wish --under)
---

<free-form prose body, no headers, articulating the path-cost analysis:>

<- the shape (what would we build along this path)>
<- build cost: LOC + infrastructure + ADR work>
<- maintenance cost: ongoing surface, cognitive overhead, who has to remember>
<- reversibility: cost of changing our minds later>
<- forward optionality: what this path preserves vs forecloses>
<- cognitive load: new concept vs reused pattern>
<- what it preserves (good downstream consequences)>
<- what it forecloses (bad downstream consequences)>
<- why rejected as the chosen path — names the dimension where the chosen wins>
```

Alternatives stay shallow unless drilled. `/wish --under <alt-path>` walks the alternative deeper to compare its tree against the chosen path's tree.

### Reading a tree

The user reads top-down:

- `index.md` lists roots
- Each root's `wish.md` describes the vision-level shape + named alternatives + sub-wishes
- Drill in to read sub-wishes
- Drill into `alternatives/` to compare counter-paths
- A leaf wish reads as a plan. Walking from root to leaf reads as a design unfolding.

## Invocation

```
/wish <theme>                       walk wish trees from a theme
/wish --as <role>                   from a role's perspective
/wish <theme> --as <role>           combine
/wish --compare <wish-path>         materialize the alternatives named in <wish>'s prose
                                    as shallow sibling files under alternatives/
/wish --under <wish-path>           drill into one subtree (chosen or alternative)
                                    from a prior pass
```

No `--depth`, no `--count` flags. Depth is whatever the chains produce; count is whatever survives producibility + filter.

Roles: `player`, `designer`, `agent`, `operator`, `developer`. Skip `substrate-developer` — engine-internals work isn't surfaced as a use-case wish.

## Steps the agent runs

### 1. Pre-load adversity sources

Read, selective on the theme:

- `MEMORY.md` index; open relevant entries (parked, deferred, vision, friction, papercuts, commitments).
- `CLAUDE.md` — current architectural state + "Notes on …" prose where friction patterns surface.
- `docs/adr/` — *Rejected alternatives* and *Future work* sections; parked aspirations live there.
- `gh issue list --state open --limit 100 --json number,title,body,labels`.
- `~/.claude/projects/<project-slug>/capture/log-*.jsonl` (`<project-slug>` = the Claude Code project directory that holds your auto-memory `MEMORY.md`) — grep `"I wish"` literally; scan for repeated tool calls hitting the same wall, manual workarounds, frustration markers.
- `git log --oneline main | head -50`.
- Empathy material (especially for non-agent roles): general knowledge of how the role works in similar engines/contexts, the user's published vision in memory entries, domain patterns from training.

**For concrete engine surfaces, the code is authoritative.** `CLAUDE.md`, ADRs, and memory orient you to what exists *in spirit*, but they drift. The moment a wish is about to compose on a specific kind / cap / mailbox / trait / path / signature, `grep` the crates (`crates/aether-*/src/`) to confirm the real name and shape before writing it in. Adversity sources tell you *why*; the code tells you *what is actually there* (per *Wishes ground their shape in real code*).

### 2. Generate roots from adversity

1-3 root wishes from the adversity corpus. Each root names an outcome the role wants to achieve. For non-agent roles, anchor empathy on the user's stated commitments (`project_avatar_commitment`, `project_mmo_vision`, etc.) — empathy from "users in general" produces shapeless wishes.

Root wishes must name an outcome someone runs aether to *achieve*, not a tool. If a candidate root reads as *"I wish [tool] existed,"* restate as the outcome the tool serves.

### 3. Drill each root

For each root, recursively:

1. **Articulate the shape at this depth.** LOD-appropriate.
2. **Producibility check.** Can this shape be written with known means within current resources? Yes → wish is a plan (no children). No → identify absences → each becomes a sub-wish.
3. **Name alternatives in prose.** What other shapes were considered? Each with a one-line shape + one-line path-cost sketch. Don't materialize as files yet (that's `/wish --compare`).
4. **Name doors opened and doors closed.** Two short paragraphs. What does this choice unlock? What does it commit to?
5. **Coherence check.** Children must compose into the parent's plan; if a child wouldn't satisfy the parent, restate.
6. **Recurse on sub-wishes** until each leaf is producible.

### 4. Filter against existing work (tree-aware)

**Leaf wishes** (the plans at the bottom): if covered, drop.

- Open issues with the same mechanism ⇒ drop, note in considered-and-dropped.
- ADRs (parked/merged) covering this mechanism ⇒ drop.
- Recent commits — just-shipped ⇒ drop.

**Interior wishes** (in-the-tree wishes that have children): if covered, **link rather than drop**.

- Aspirational memory entries ⇒ keep wish; add `supports: [[memory-entry-name]]` to frontmatter.
- Memory entries with explicit parking + no forcing function ⇒ drop.

**Resource check** at every level: if producibility requires unrealistic compute/money, drop or flag.

### 5. Materialize alternatives on `--compare`

When `/wish --compare <wish-path>` is invoked:

1. Read the named alternatives from the wish's prose body.
2. For each, create `<wish-path>/alternatives/<slug>/wish.md` with the alternative wish.md shape (frontmatter + path-cost prose).
3. Each alternative file is depth 1 — no children. The shape is articulated; the build/maintenance/reversibility/forward-optionality/cognitive-load analysis is in prose.
4. Don't filter alternatives against existing work — they're counter-paths for comparison, not candidates to file.

If alternatives are already materialized, refuse with *"Alternatives already exist under <path>; use /wish --under <alt-path> to drill one."*

### 6. Drill an alternative on `--under`

When `/wish --under <alternative-path>` is invoked: walk the alternative as if it were a chosen wish — generate its own sub-wishes, possibly its own sub-alternatives, recurse to producibility. The result is a competing subtree the user can compare against the original chosen path's subtree.

### 7. Write the tree

Output root: `wishes/<YYYY-MM-DD>-<theme-slug>/`. Write `index.md` and each `wish.md`.

`index.md` includes:

- Date, theme, role
- Sources scanned summary
- Adversity-source breakdown (data vs empathy counts)
- List of root wishes
- Total wishes, leaf/plan count, interior/wish count, max/min depth
- Alternatives count (materialized vs only-named-in-prose)
- Considered-and-dropped list
- Notes

### 8. Report to user

```
✓ Wish pass complete.
  Theme: <theme>[, as <role>]
  Tree: <N> roots, <M> total wishes across <K> depth levels
  Plans (leaves): <P>     Wishes (interior): <I>
  Alternatives: <named-in-prose> named, <materialized> materialized
  Adversity sources: <data-count> data-grounded, <empathy-count> empathy-modeled
  Considered+dropped: <D> (incl. <R> resource-bound)
  Index: wishes/<date>-<theme>/index.md

Read the index, drill into the wishes that interest you.
Materialize a wish's alternatives: /wish --compare <wish-path>
Drill an alternative or sub-wish deeper: /wish --under <wish-path>
File leaf plans you want to commit to as Backlog-Phase issues.
```

## Path-cost dimensions (canonical set)

Every alternative wish carries all five:

- **Build cost** — LOC, design time, new infrastructure required, ADR work.
- **Maintenance cost** — ongoing cognitive surface, breaking-change discipline, who has to remember it.
- **Reversibility cost** — if we change our minds, how expensive is the migration?
- **Forward optionality** — what future paths the choice preserves vs forecloses.
- **Cognitive load** — new concept vs composes with existing patterns.

The "why rejected as the chosen path" line names which dimension(s) the chosen wish wins on.

## What `/wish` does NOT do

- File issues (the user triages).
- Write production code (that's `/implement`).
- Materialize alternatives unless `--compare` is invoked.
- Drill alternatives unless `--under` is invoked on the alternative.
- Pad the tree to a count or depth target.
- Wish for things already producible — those are plans, not wishes; write them directly as the wish's own body.
- Wish for resource-infeasible shapes — drop or flag.
- Speculate without grounding — every wish carries data or empathy source.
- Claim an existing engine surface (kind / cap / mailbox / trait / path / signature) without grep-confirming it against current code. The novel wished thing is invented; the means it builds on are *verified*, never recalled.

## Failure modes

- **Theme too broad**: refuse with *"Theme too broad — try a narrower scope."*
- **Empty adversity corpus**: return 0-1 roots and a notes paragraph.
- **`gh` rate-limited**: degrade gracefully, note partial scan.
- **Producibility check fails for the entire root** (resource-infeasible): mark `producible: false` + `resource_bound: true`; document. Don't drop.
- **L1 drift** (root reads as workflow not outcome): restate as outcome before drilling.
- **Unverifiable surface a wish leans on**: don't substitute a plausible name. Either the surface doesn't exist (drill the absence) or it's renamed (grep for the real name). A guessed surface makes producibility a lie.
- **Children don't compose to satisfy parent**: back up, restate.
- **`--compare` invoked on a wish with no named alternatives**: write a note in the wish's prose suggesting alternatives, refuse with *"No alternatives named in <wish-path>. Add alternatives in the body and re-invoke."*
- **`--under` invoked on a path that doesn't exist**: refuse with the valid prior-pass paths.

## Output gitignore

`/wishes/` is gitignored (per-run scratch).
