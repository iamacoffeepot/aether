---
name: scope
description: Walk a GitHub issue Define → Design → Plan and update the release Project board. Stops at Plan with AgentReady=No, awaiting user approval via /approve. Aggressive (agent makes design calls) and resumable (per-phase updates to the board).
---

# /scope — release scoping skill

Walk a single issue from Backlog through Define → Design → Plan, producing a problem statement, design rationale, and implementation plan as structured body sections. Updates the active release Project's `Phase` field as it advances. Stops at `Plan` with `AgentReady=No`, awaiting user review via `/approve`.

This skill produces **scoping artifacts only**. It does not write production code (that's `/implement`), open implementation PRs, or set `AgentReady=Yes` (that's `/approve`).

## Invocation

```
/scope <issue-number>                fresh run, or resume from current Phase
/scope <issue-number> --phase define rewrite Define section, redo downstream
/scope <issue-number> --phase design rewrite Design section, redo Plan
/scope <issue-number> --phase plan   rewrite Plan section only
```

## Configuration

Reads `.claude/release-state.json` at the repo root:

```json
{
  "active_project": 2,
  "project_node_id": "PVT_kwHOC4r7e84BYJmX",
  "release_version": "0.4",
  "owner": "iamacoffeepot",
  "field_cache": {
    "Phase":      { "id": "PVTSSF_...", "options": { "Backlog": "...", "Define": "...", ... } },
    "Type":       { "id": "...", "options": { ... } },
    "Size":       { "id": "...", "options": { ... } },
    "AgentReady": { "id": "...", "options": { ... } },
    "BounceTo":   { "id": "...", "options": { ... } }
  },
  "item_cache": { "<issue-number>": "PVTI_..." }
}
```

If the file is missing, abort with: *"No active release state. Run `/release-init <version>` first or create `.claude/release-state.json`."*

`field_cache` is populated by `/release-init`; no other skill writes it. `item_cache` maps issue numbers to project item IDs — `/sketch` seeds it at filing, and any skill appends on a lookup miss (see [Project board mechanics](#project-board-mechanics)). Item IDs are stable for the life of the project.

## Phase walk

For each sub-phase: read inputs, write the corresponding body section, advance the `Phase` field on the project board — and reconcile the matching `phase:*` label (see [Phase label reconcile](#phase-label-reconcile)). If a sub-phase has nothing to do (already complete on a resumed run), skip it.

### Define

- **Inputs**: issue body, comments, linked issues (`gh issue view <n> --json body,comments,closingIssuesReferences`).
- **Output**: replaces or adds `## Problem statement` section in the body. Two short paragraphs:
  1. *What's being solved.* The concrete problem in plain language.
  2. *Why now / success criteria.* Why this release; what "done" looks like observably.
- **Bounce**: if the body is too vague to frame a problem (e.g. one-line title, no description, no linked context), self-bounce with `Phase=Bounced`, `BounceTo=Define`, and a comment asking the specific clarifying question. Don't guess.
- **Project board**: set `Phase=Design` on success.

### Design

- **Inputs**: this issue's body so far, related ADRs in `docs/adr/`, the auto-memory directory, relevant code in the affected crates (read via Read tool, not just grep).
- **Posture**: aggressive — the agent picks between roughly-equal options and notes the rejected one. Self-bounce only when options are truly tied *or* the right answer needs information only the user has.
- **Output**: replaces or adds `## Design notes` section. Structure:
  ```
  ### Chosen approach
  <one paragraph: what we'll do and why>

  ### Rejected options
  - **<option A name>** — why not (one line)
  - **<option B name>** — why not (one line)

  ### Affected surfaces
  <crates, public traits, wire formats, ADRs touched>
  ```
- **ADR drafting**: if the chosen approach is load-bearing (touches public traits, wire formats, lifecycle, dispatch, or otherwise looks architectural), scaffold an ADR via the `/adr` skill on a `docs/adr-NNNN-<slug>` branch and open a PR. Link the ADR PR in the Design notes section. The issue is not eligible for `/approve` until the ADR PR is merged.
- **Bounce**: rare. Use only when truly stuck on a value-judgment the user must make.
- **Project board**: set `Phase=Plan` on success.

### Plan

- **Inputs**: this issue's body (Define + Design must be present), the affected files (deeper read than Design — look at the actual code that will change).
- **Output**: replaces or adds two sections:

  ```
  ## Implementation plan

  1. <step> — <files touched> — <test coverage>
  2. <step> — ...
  ```

  And, if the work spans multiple PRs:

  ```
  ## Sub-issues

  - #NNN <child issue title>
  - #MMM <child issue title>
  ```

- **Multi-PR split** triggers when:
  - More than 3 logically-separable changes, *or*
  - More than 2 crates with logically-separable work
  - In that case, file each sub-issue via `/sketch`'s mechanics (Phase=Backlog initially, link parent), update §Sub-issues, and scope each child in a follow-up `/scope` run.
- **Size estimation** — set the `Size` custom field on the project item:
  - **S**: single file, single concept, <100 LOC change
  - **M**: single crate, multiple files, <500 LOC change
  - **L**: cross-crate, architectural, or >500 LOC change
- **Project board**: leave `Phase=Plan`; set `AgentReady=No` (default, but explicit). This is the resting state awaiting `/approve`.

## Side findings

During Design and Plan reads, the agent will inevitably notice unrelated issues — dead code, undocumented invariants, latent bugs in adjacent files. Don't chase them. Add a body section:

```
## Side findings

- <one-line description> — <pointer: file:line or crate>
- ...
```

These are *not auto-filed* as child issues. The user reviews them at `/approve` time and chooses which to spin off via `/scope-spinoff <issue>` (separate skill).

## Comments

No progress comments — phase transitions are already legible from the `phase:*` labels (the issue timeline records every label change), the board fields, and the body sections themselves. A comment exists only when it is addressed to a human and carries content with no structured home. For this skill that is the **self-bounce question/blocker**, written as prose markdown with a bold lead, no `[skill]` prefix:

```markdown
**Bounced to Define** — the body doesn't say which chassis this applies to.

Desktop-only (window mail exists) or all four? The answer changes whether the
headless chassis needs a nop mailbox. Re-run `/scope` once the body says.
```

Don't pad the comment with summaries of work that completed — the body sections carry that.

## Body editing mechanics

`gh issue edit <n> --body <text>` replaces the entire body. To avoid clobbering user-written content:

1. Read the current body.
2. Identify scope-managed sections by their H2 headers: `## Problem statement`, `## Design notes`, `## Implementation plan`, `## Sub-issues`, `## Side findings`. Everything else is user content; preserve verbatim.
3. Insert or replace the managed sections, preserving user content above and below them.
4. Write back via `gh issue edit`.

The agent must be careful here — the body is the canonical scope artifact and clobbering user-written background notes will erode trust.

## Project board mechanics

To update a single-select field on a project item:

```
gh project item-edit \
    --id <item-id> \
    --project-id <project-node-id> \
    --field-id <field-id> \
    --single-select-option-id <option-id>
```

Field and option IDs come from `.claude/release-state.json`'s `field_cache`; the item ID comes from its `item_cache` (keyed by issue number). On a cache miss, resolve it with one targeted GraphQL query — never a board-wide `gh project item-list` scan:

```bash
gh api graphql -f query='query { repository(owner: "<owner>", name: "aether") {
  issue(number: <n>) { projectItems(first: 10) { nodes { id project { id } } } } } }'
```

Pick the node whose `project.id` matches `project_node_id`, then append it to `item_cache` so every later run skips the query.

GitHub's REST and GraphQL rate budgets are separate, and the GraphQL one is the binding constraint when many agents run — spend GraphQL only where it's mandatory (board field reads/writes); labels, comments, and body edits ride REST via plain `gh issue` commands.

## Phase label reconcile

The board `Phase` field is only visible on the project board — not on the issue itself or in `gh issue list`. This skill mirrors every Phase write as a `phase:*` label on the issue so the lifecycle is legible at a glance, and the label never disagrees with the board. **In the same step you set the `Phase` field, reconcile the label:**

```bash
gh issue edit <n> --remove-label "phase:define,phase:design,phase:plan,phase:ready,phase:executing,phase:refine,phase:bounced,phase:stalled" \
  && gh issue edit <n> --add-label "phase:<new>"
```

`--remove-label` ignores labels the issue doesn't carry, so the remove is safe on any transition and idempotent on re-run (lowercased: `Phase=Ready` → `phase:ready`). The two calls are chained with `&&` so the add fires only after the remove succeeds — if the first `gh` call stalls or errors (a transient CLI or API outage), the chain stops there instead of stamping the new label onto an issue whose old phase label is still present, which would leave two phase labels on the board at once. A reconcile that fails partway leaves the prior label untouched and heals on the next run. Two phases carry no label — `Backlog` (the resting/default state) and `Done` (the issue is closed); when moving to either, drop the `&&` add and run the remove alone. For this skill the writes are `Phase=Design`, `Phase=Plan`, and (on self-bounce) `Phase=Bounced`.

## Restart and resume semantics

- **Fresh `/scope <issue>`**: detect current `Phase` from the board. Run only the sub-phases that haven't completed. A completed sub-phase is one whose body section is present and non-empty.
- **`/scope <issue> --phase <name>`**: force rewrite of that sub-phase's section regardless of completion. Downstream sub-phases re-run because their inputs changed. (E.g. redoing Design implies redoing Plan because Design choices drive Plan steps.)
- **After a bounce**: the user resolves the bounce (clarifies the issue, picks the tied option), then re-invokes `/scope <issue>` to resume from the bounced phase.

## What `/scope` does NOT do

- Write production code (use `/implement` after `/approve`).
- Open implementation PRs (use `/implement`).
- Merge anything.
- Auto-file side findings as child issues (use `/scope-spinoff`).
- Set `AgentReady=Yes` (use `/approve`).
- Set `Type` — that should come from the issue title's conventional-commit prefix (`feat:` → `feat`, `fix:` → `fix`, etc.). If unset, infer from title; if title has no prefix, leave unset and surface it in the run's output.

## Failure modes to handle gracefully

- **`.claude/release-state.json` missing**: abort with the helpful message above.
- **Issue not in active project**: add it (`gh project item-add`), then proceed.
- **`gh` lacks `project` scope**: abort with *"Run `gh auth refresh -s project`"*.
- **Issue already at Phase=Done or Phase=Executing**: refuse with *"Issue is past Plan — use `/bounce` to regress or work in a fresh issue."*
- **ADR drafting failure**: keep the issue at Design, explain in the run's output, don't advance to Plan.
- **Body-edit collision (user edited body mid-run)**: re-read, re-merge, surface any conflicts in managed sections in the run's output.
