---
name: retrospect
description: Review a session's activity, triage the tooling and process papercuts it surfaced into repo-actionable vs. self-inflicted, print the numbered plan, wait for one confirmation, then file the actionable ones as papercut-labelled Backlog issues via /sketch's mechanics. At an arc boundary it adds a cross-slice design-debt pass — reading across a completed arc's PRs for concepts that accreted one slice at a time and now want reifying. v1 is session-only; release and week levels are named but deferred.
---

# /retrospect — session papercuts → filed issues

Gathering the tooling and process papercuts hit during a session, triaging the repo-actionable ones from the self-inflicted ones, and routing the keepers through the issue pipeline is a ritual that bites every time it is done by hand. `/retrospect` turns that ritual into one confirmed operation: enumerate, classify, confirm, file.

`/retrospect` composes with `/sketch` — it adds the triage step and the `papercut` label and defers all issue-filing mechanics to `/sketch`. The goal is a filed record of the session's actionable friction, not a replacement for scoping or design.

`/retrospect` runs two enumerations over different inputs into one shared gate. The **session pass** (always) gathers the papercuts the session hit. The **design-debt pass** (at an arc boundary — most naturally the moment an ADR flips to Accepted, which the flips-Accepted-after-implementation convention makes the arc-complete checkpoint) reads across the finished arc's PRs for concepts that emerged one slice at a time and now want reifying. Per-PR review structurally cannot see that shape — each slice is locally reasonable, and the debt only reads as debt once the whole arc is in view. Both passes feed the same classify → confirm → file flow below.

## Invocation

```
/retrospect [session]               review the current session's activity (default)
```

`session` is the only implemented level. The `{level}` argument slot is reserved for future levels (`release`, `week`) that aggregate across sessions — both are out of scope for v1 because they draw on a different input (multiple transcripts or a time window) than a single session's enumeration. Passing an unrecognized level is a hard stop; see [Failure modes](#failure-modes).

## Preconditions

1. The running session has reviewable activity — at least one exchange in which tooling, process, or project mechanics were encountered.

## The flow

### 1. Enumerate candidates

Two inputs feed the candidate list; a candidate from either needs only a sentence of context at this stage — full scoping happens later via `/scope`.

**Session papercuts (always).** Review the current session for tooling friction, process gaps, project gotchas, harness rough edges, and workflow inefficiencies. Cast the net broadly: anything that caused confusion, required a workaround, surfaced a missing guardrail, or is worth a note goes on the candidate list.

**Cross-slice design debt (arc boundary).** When the session sits at an arc boundary — most naturally the moment an ADR flips to Accepted — read across the arc's implementation PRs (the slices that landed the ADR) and ask what concepts emerged across them that now want reifying. The four shapes worth naming:

- a **string vocabulary that accreted one entry per slice** — a family of string constants that grew by one with each PR and now wants to be an enum or a typed id (the bloomery topic vocabulary that motivated this pass grew a constant per slice from ADR-0149 through ADR-0153, and no single slice owned the "should this be a type?" question);
- a **field whose doc comment disambiguates per-case meanings** — the comment enumerates what the value means in each situation, which is a sum type in disguise;
- an **integer discriminant** standing in for a closed set of cases that a named type would carry;
- an **invariant documented but nowhere checked** — a rule stated in prose or a comment that no code enforces.

Each hit is a candidate, described in a sentence that names the concept and the slices it spans.

### 2. Classify each candidate

For each candidate, apply the same judgment a human reviewer would at triage:

- **File** — the root cause is a gap in the project (missing lint, broken script, undocumented constraint, harness papercut, CI gap). Someone else hitting the same session could plausibly hit this too. The fix belongs in the repo.
- **Skip** — the friction was self-inflicted (misread a doc that exists, ran the wrong command, misunderstood a Rust concept), or is purely personal workflow, or is already tracked. Record the candidate and the skip reason; do not file.

A design-debt candidate takes the same File/Skip judgment through a reifying lens: **File** when the concept genuinely wants a type and a later slice would otherwise pay the debt again; **Skip** when the vocabulary is closed and stable, the disguise is deliberate, or a type already carries the distinction. Record the reason either way.

Every candidate gets an explicit disposition — no silent drops.

### 3. Print the plan and wait for confirmation

Print the full classification before touching anything:

```
Retrospective — <N> candidates

  File:
    1. <title inference> — <one-line reason>
    2. <title inference> — <one-line reason>

  Skip:
    3. <description> — self-inflicted: <reason>
    4. <description> — already tracked: #<N>

File issues 1–2? (y to proceed, or edit the list first):
```

Wait for exactly one response. The user may confirm with `y`, adjust the list (remove items by number, change a disposition), or cancel. Do not auto-proceed.

### 4. File the actionable picks via `/sketch`

For each confirmed-file candidate, file via `/sketch`'s mechanics (read `.claude/skills/sketch/SKILL.md` — it is the single definition of issue filing). Pass `--label papercut` on each session-papercut candidate; the `papercut` label already exists in the repo. A design-debt candidate is reification/refactor work rather than a papercut, so file it without the `papercut` label and let `/sketch` infer the type and scope (usually `refactor`). Backlog is label-absence — no `phase:*` label is added.

The issue title follows `/sketch`'s conventional-commit form (`type(scope): subject`). Infer type and scope from the candidate's description using `/sketch`'s inference table. If the scope is ambiguous, ask inline before filing — a wrong scope is worse than one question.

Body template:

```markdown
## Description

> <candidate description, as enumerated>

<2–3 sentences of grounding: what part of the system this touches, any file pointer
already in hand, the session context that surfaced it. Nothing speculative.>

## Found during

Filed from `/retrospect session` on <date>.
```

For a design-debt candidate, name the arc instead of the session: `Filed from /retrospect's design-debt pass at the ADR-NNNN arc boundary on <date>.`, and list the slice PRs the concept spans in the grounding paragraph.

No `## Problem statement` / `## Design notes` / `## Implementation plan` — those are `/scope`'s sections. No audit comment — the issue creation event is the record.

## Output

After all filings complete:

```
✓ Filed #<N>: <title>
✓ Filed #<N>: <title>

Skipped:
  - <description> (self-inflicted: <reason>)
  - <description> (already tracked: #<N>)

Next: /scope <N> when any of the above is ready to be worked.
```

## Failure modes

- **No actionable candidates**: print the full classification (all skips), report `Nothing to file.`, stop.
- **Level other than `session` requested** (e.g. `/retrospect release`): refuse with *"`release` is a deferred level — only `session` is implemented in v1."* Do not attempt the enumeration.
- **Filing partway through fails** (e.g. GitHub rate limit between issues 1 and 3): commit completed work — already-filed issues stay filed. Report which succeeded and which failed; the user re-runs with the remaining candidates once the cause is resolved.
- **Scope ambiguous**: ask inline before filing. One question is less friction than a misfiled issue.
- **No session activity reviewable**: refuse with *"Nothing to retrospect — the session has no activity to review."*

## What `/retrospect` does NOT do

- Scope, design, or plan the filed issues. Each filed issue is Backlog; run `/scope <N>` when it is ready.
- Auto-file without confirmation. The one-confirmation gate is load-bearing: triage is judgment-heavy, and the skip list is as important as the file list.
- Aggregate across sessions in v1. Cross-session levels (`release`, `week`) are explicitly deferred. The design-debt pass reads an arc's PRs on GitHub, not a window of session transcripts — it is arc-scoped, not a cross-session level.
- Modify existing issues, comments, or labels on the parent session's tracked work.
- Open PRs or write production code.
