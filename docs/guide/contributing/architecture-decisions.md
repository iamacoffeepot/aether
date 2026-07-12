# Architecture decisions

Architecture Decision Records are Aether's durable memory for load-bearing
choices. They record why a boundary exists, what was chosen, the consequences
accepted, and which alternatives lost. They are versioned and reviewed beside
the code so a later contributor can judge whether the original forces still
apply.

An ADR is not a task tracker, API reference, or claim that every described phase
has shipped. Keep decision status and implementation reality separate.

## What each source answers

| Source | Question |
|---|---|
| Current code and tests | What behavior exists now? |
| Accepted ADR and supersession chain | Why is the load-bearing design this way? |
| Proposed ADR | What decision is under review, not yet governing? |
| GitHub issue and pull request | What work was planned, reviewed, or landed? |
| This guide | How do the decisions compose into a usable mental model? |

If an ADR describes old crate names, APIs, or phasing, preserve its historical
text. A later ADR, code anchor, or guide page should provide the forward pointer.
Do not rewrite past reasoning until it looks current.

## Status and implementation are different axes

The [ADR template](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/TEMPLATE.md) offers `Proposed`, `Accepted`, and
`Superseded by ADR-XXXX`. The existing log also contains historically qualified
states such as Rejected or Overturned. Read the literal status line and any
forward notes; do not normalize an unfamiliar status by guesswork.

| Decision status | What it means | What it does not prove |
|---|---|---|
| Proposed | The decision is available for review | That its policy governs or its implementation may be assumed |
| Accepted | The decision was adopted | That every optional phase or future consequence shipped |
| Superseded | The historical reasoning remains, but a named later decision governs the replaced part | That every sentence in the old ADR is false |
| Rejected or Overturned | The recorded direction was declined or later reversed | That the context and alternatives have no historical value |

Implementation can be absent, partial, complete, or later removed under any
historical record. An Accepted ADR may explicitly defer phases. Code may
experiment with part of a Proposed ADR without silently accepting its entire
policy. A shipped subsystem can later be removed while its ADR remains as the
record of why it once existed.

For immediate behavior, trust current code and tests. For current architectural
intent, follow the Accepted, non-superseded chain. When status and realization
appear inconsistent, report both rather than promoting a status or rewriting
history incidentally.

## When a decision needs an ADR

Use an ADR for a choice future contributors must understand before safely
changing it. Strong signals include a new or changed:

- public trait or cross-crate dependency boundary;
- wire format, schema identity, or compatibility contract;
- actor lifecycle, scheduling, or dispatch model;
- addressing, identity, or name-resolution rule;
- native/wasm or trust boundary;
- persistent architectural policy with meaningful rejected alternatives.

Ordinary implementation notes, a localized bug fix, a mechanical refactor
inside an accepted boundary, and a reversible detail with no durable tradeoff
do not need an ADR merely to make the change look important.

During [scoping](agent-workflow.md), Design identifies the boundary. It should
cite an applicable ADR already present, link an ADR draft already under review,
or stop at Design and hand off creation of a new draft. Approval does not treat
an unmerged architectural decision as settled.

## Finding the governing decision

Do not read the ADR directory only from number one forward. Start from the
surface you intend to change:

1. Read the relevant guide foundation, system page, or recipe and follow its
   governing-ADR links.
2. Search the affected crate and public types for `ADR-NNNN` comments.
3. Search ADR titles and Context sections for the subsystem, mailbox, kind,
   trait, or boundary.
4. Read the status line before relying on the decision.
5. Follow every “superseded by,” “amends,” and partial-supersession pointer in
   both directions.
6. Compare the decision's current claims with code and tests.

Sequential numbering is chronology, not taxonomy. A later ADR may amend only
one clause of an earlier one, and a consolidation ADR may be the forward map
for many historical paths.

The [architecture overview](../architecture.md), [subsystem map](../systems.md),
and [invariants](../foundations/invariants.md) are useful entry points. They
digest decisions; they do not replace reading the governing record when making
a load-bearing change.

## Drafting a decision

The Codex [`adr` skill](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/adr/SKILL.md) scaffolds a new,
sequentially numbered Proposed ADR in a dedicated worktree. It does not invent
the decision, mark it Accepted, commit it, or merge it. The user supplies the
substance and the ADR is reviewed through a normal pull request.

Every ADR begins from the checked-in
[template](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/TEMPLATE.md) and answers:

- **Context:** What problem, constraints, and prior decisions create the choice?
- **Decision:** What is chosen, stated plainly?
- **Consequences:** What becomes easier, harder, required, or foreclosed?
- **Alternatives considered:** What credible options lost, and why?

Write enough Context that a future reader can decide whether the forces still
hold. In Decision, distinguish required invariants from illustrative
implementation. In Consequences, name migration or compatibility costs rather
than hiding them in an issue plan.

An ADR can cite implementation issues and planned phases, but it should remain
legible after those issue threads age. The issue owns the executable work plan;
the ADR owns the durable choice.

## Superseding without erasing

When a later decision replaces an earlier one:

- leave the original reasoning intact;
- update its status or add a precise forward note;
- name the successor;
- state whether the replacement is whole or partial;
- have the successor acknowledge what it replaces;
- update guide entry points to send new readers to the governing chain.

Partial supersession needs a boundary. “Superseded in part” without naming the
surviving and replaced clauses forces every future reader to reconstruct the
decision from code archaeology.

Do not use a source-code migration as an excuse to rewrite historical crate
names throughout old ADRs. Put the current layout in a consolidation decision
or guide map and link back.

## ADRs in code and guide prose

An ADR citation should support a load-bearing claim, not decorate a paragraph.
Place it next to the invariant, boundary, or non-obvious choice it explains.
Prefer the most specific governing ADR and include an amended predecessor only
when its surviving context matters.

Code comments should cite an ADR where a future “cleanup” could otherwise erase
an intentional oddity: a must-stay dependency, wire-shape pin, trust boundary,
or lifecycle exception. The comment should state the local invariant as well as
the number; a bare `ADR-0123` gives a reader no clue what to preserve.

Guide prose should explain the composed model and link the decision. It should
not paste the ADR's Context, Decision, and Alternatives into a second document
that will drift independently.

## Auditing drift

Treat these as different findings:

- **Stale guide:** prose or a recipe names a removed path or signature.
- **Stale ADR realization note:** status or phasing claims contradict current
  code or merged evidence.
- **Architecture drift:** current code violates an Accepted, governing decision.
- **Undocumented new decision:** code establishes a load-bearing boundary with
  no applicable ADR.

The remedy depends on the classification. Fixing a guide path does not require
changing an ADR. Correcting a factual realization note must preserve historical
reasoning. Architecture drift may require code changes, a superseding ADR, or a
deliberate user decision; do not choose among those silently.

The [`sweep adrs` workflow](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/sweep/SKILL.md) can perform
a read-only status audit and present evidence. Status edits remain ordinary
reviewed repository changes, never an automatic consequence of a grep hit.

## Review checklist

Before treating an ADR as governing:

- Is its status Accepted rather than merely Proposed?
- Does a later record supersede or amend the relevant clause?
- Are its crate and symbol references historical or current?
- Does current code implement, partially implement, or contradict it?
- Is the claim I am making architectural, or merely an API detail?

Before proposing a new ADR:

- Is the choice load-bearing and durable?
- Have I read the existing chain and credible alternatives?
- Can the decision be stated independently of one issue's implementation plan?
- Are consequences and compatibility costs explicit?
- Will a future reader know when the decision no longer applies?

For documentation mechanics around ADR links, continue to
[Contributing to the documentation](documentation.md).
