---
name: sketch
description: Capture an idea as a well-formed GitHub issue — lint-clean conventional-commit title, type/crate labels, no phase:* label (Backlog is label-absence). Light expansion only — preserves the user's words verbatim and adds brief context, no design or architecture reasoning (that's /scope). The single definition of "file an issue correctly"; /scope-spinoff delegates here.
---

# /sketch — idea → issue

The entry point of the pipeline: turn a rough idea into a filed, labeled issue that `/scope` can pick up later. `/sketch` is capture, not scoping — it preserves the user's intent and adds just enough context for a future reader, then stops.

This skill is the single definition of issue-filing mechanics (title, labels). Other skills that file issues (`/scope-spinoff`, `/scope`'s multi-PR split) follow this skill's mechanics rather than defining their own.

## Invocation

```
/sketch <idea text>                  file an issue from the idea
/sketch <idea text> --type <t>       override the inferred type prefix
/sketch <idea text> --crate <c>      override the inferred crate scope
/sketch <idea text> --label <l,...>  extra labels (e.g. papercut)
```

With no idea text, ask what the idea is — don't guess from conversation context unless the user just described it.

## Title

`{type}({crate}): subject` — lowercase subject, conventional-commit form. The repo's issue lint auto-applies `invalid-title` to anything else, and the `type:*` label is stamped from the prefix, so the title is load-bearing.

Type inference from the idea text (same table `/scope-spinoff` uses). The authoritative type set is the `TYPES` array in `.github/workflows/issue-labels.yml` — check it when this table and the lint diverge.

- "dead code", "unused", "drift", workflow/tooling → `chore`
- "flaky", "contention", "intermittent" → `flake`
- "missing test", "test gap", "harness tooling" → `chore` (or `fix` when the gap is a defect in existing tooling)
- "doc gap", "missing rustdoc", guide/ADR work → `docs`
- "bug", "regression", "broken", "panics" → `fix`
- new capability, "add", "support" → `feat`
- "slow", "perf regression", throughput/latency → `perf`
- restructure with no behavior change → `refactor`
- CI pipelines, runners, release automation → `chore(ci):` or `fix(ci):`
- Default if genuinely ambiguous → `chore`, and say so in the output

The crate scope comes from whatever the idea names or points at (a file path resolves to its crate; skill/workflow/process work uses `workflow`; repo-wide chores use `repo`). If the scope is ambiguous, ask inline — a wrong scope is worse than one question.

## Labels

Apply the labels on the issue-create call itself — the REST `POST …/issues` form (see [Filing](#filing)) takes them inline via repeated `-f 'labels[]=…'`, so they land in the one create request rather than as follow-up edits.

- `type:<t>` mirrors the title prefix. All seven allowed types have a matching label: `type:feat`, `type:fix`, `type:chore`, `type:docs`, `type:perf`, `type:refactor`, `type:flake`.
- `crate:<short>` for the crate scope. **If the crate is new and has no label, create it first** (`gh label create "crate:<short>" --color bfdadc --description "<full crate name>"`) — PRs against a crate with no label trip the title lint (Pattern E).
- `papercut` when the idea is a gotcha/rough-edge — pass via `--label` or infer when the idea text says so.
- No `phase:*` label — Backlog carries none by convention.

## Body — light expansion

The body preserves the user's words and adds brief grounding. It does **not** design, does not scope out what needs wiring where, and never pre-creates the `/scope`-managed sections (`## Problem statement`, `## Design notes`, `## Implementation plan`, `## Sub-issues`, `## Side findings`).

```markdown
## Description

> <the user's sketch, verbatim>

<2–3 sentences of context: what part of the system this touches, file pointers
(`crate/src/file.rs`) if already known from the conversation, and any constraint
the user stated. Nothing speculative — if you'd have to open files to say it,
leave it for /scope.>
```

The blockquote/expansion split is deliberate: `/scope`'s Define phase needs to know what is user intent versus added context.

Callers delegating to this skill (e.g. `/scope-spinoff`) may append their own sections after `## Description` (such as `## Found during`); `/sketch` itself adds nothing more.

## Filing

File over REST — `gh issue create` is GraphQL-backed, while `POST …/issues` is REST, so filing stays off the contended GraphQL pool. Write the body to a file so backticks / `$` in it aren't shell-expanded, and pass the labels inline:

```bash
gh api -X POST repos/iamacoffeepot/aether/issues \
  -f title="<type>(<crate>): <subject>" \
  -F body=@/tmp/sketch-body.md \
  -f 'labels[]=type:<t>' -f 'labels[]=crate:<c>' \
  --jq '.number'
```

A freshly filed issue carries no `phase:*` label, which **is** its Backlog state — Backlog is label-absence, the convention every pipeline skill reads (`/scope`'s sweep discovers Backlog as "open, non-PR, no `phase:*` label"). `type:*` rides the issue from filing; `/scope` stamps `size:*` / `model:*` at Plan. No audit comment — the issue's own creation event is the record.

## Output

```
✓ Filed #<N>: <title>
  Labels: <labels>
  Next: /scope <N> when it's ready to be worked.
```

## Failure modes

- **Crate scope ambiguous**: ask inline before filing. Don't file with a guessed scope.
- **Idea is really several ideas**: file one issue per separable idea, confirming the split with the user first.

## What `/sketch` does NOT do

- Scope, design, or plan — no architecture reasoning, no reading code beyond pointers already in hand. `/scope` does the thinking.
- Pre-create scope-managed body sections.
- Stamp a `phase:*` label — a new issue is Backlog by carrying none.
- Post comments.
- Write production code or open PRs.
