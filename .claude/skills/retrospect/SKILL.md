---
name: retrospect
description: Review a session's activity, triage the tooling and process papercuts it surfaced into repo-actionable vs. self-inflicted, print the numbered plan, wait for one confirmation, then file the actionable ones as papercut-labelled Backlog issues via /sketch's mechanics. v1 is session-only; release and week levels are named but deferred.
---

# /retrospect — session papercuts → filed issues

Gathering the tooling and process papercuts hit during a session, triaging the repo-actionable ones from the self-inflicted ones, and routing the keepers through the issue pipeline is a ritual that bites every time it is done by hand. `/retrospect` turns that ritual into one confirmed operation: enumerate, classify, confirm, file.

`/retrospect` composes with `/sketch` — it adds the triage step and the `papercut` label and defers all issue-filing mechanics to `/sketch`. The goal is a filed record of the session's actionable friction, not a replacement for scoping or design.

## Invocation

```
/retrospect [session]               review the current session's activity (default)
```

`session` is the only implemented level. The `{level}` argument slot is reserved for future levels (`release`, `week`) that aggregate across sessions — both are out of scope for v1 because they draw on a different input (multiple transcripts or a time window) than a single session's enumeration. Passing an unrecognized level is a hard stop; see [Failure modes](#failure-modes).

## Preconditions

1. The running session has reviewable activity — at least one exchange in which tooling, process, or project mechanics were encountered.

## The flow

### 1. Enumerate candidate papercuts

Review the current session for tooling friction, process gaps, project gotchas, harness rough edges, and workflow inefficiencies. Cast the net broadly: anything that caused confusion, required a workaround, surfaced a missing guardrail, or is worth a note goes on the candidate list. A candidate needs only a sentence of context at this stage — full scoping happens later via `/scope`.

### 2. Classify each candidate

For each candidate, apply the same judgment a human reviewer would at triage:

- **File** — the root cause is a gap in the project (missing lint, broken script, undocumented constraint, harness papercut, CI gap). Someone else hitting the same session could plausibly hit this too. The fix belongs in the repo.
- **Skip** — the friction was self-inflicted (misread a doc that exists, ran the wrong command, misunderstood a Rust concept), or is purely personal workflow, or is already tracked. Record the candidate and the skip reason; do not file.

Every candidate gets an explicit disposition — no silent drops.

### 3. Print the plan and wait for confirmation

Print the full classification before touching anything:

```
Session retrospective — <N> candidates

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

For each confirmed-file candidate, file via `/sketch`'s mechanics (read `.claude/skills/sketch/SKILL.md` — it is the single definition of issue filing). Pass `--label papercut` on each; the `papercut` label already exists in the repo. Backlog is label-absence — no `phase:*` label is added.

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
- Aggregate across sessions in v1. Cross-session levels (`release`, `week`) are explicitly deferred.
- Modify existing issues, comments, or labels on the parent session's tracked work.
- Open PRs or write production code.
