---
name: bounce
description: Explicit phase regression. Move an issue from its current Phase back to an earlier one (Plan / Design / Define), record the reason as a comment, and set the BounceTo field so /scope can resume from the right place. Required reason — no silent bounces. For env/tooling halts, use Phase=Stalled manually (no skill in v1).
---

# /bounce — explicit phase regression

The user invokes `/bounce` when reviewing scope artifacts (or watching execution) and concludes an upstream phase needs rework. The skill records the regression as a `Phase=Bounced` + `BounceTo=<phase>` state with an audit comment. `/scope` then resumes from the target phase on next invocation.

Self-bounces by other skills (`/scope` hitting a wall, `/implement` discovering a design flaw mid-execution) use the same mechanism — this skill is the explicit user-driven wrapper.

## Invocation

```
/bounce <issue> <to-phase> --reason "<text>"
```

`<to-phase>` is one of: `Define`, `Design`, `Plan`. `--reason` is required.

## Preconditions

1. `.claude/release-state.json` exists. If not, abort with the standard pointer.
2. Issue must be in the active project.
3. `--reason` is non-empty (no silent bounces — the corpus shows bounces without context are the hardest to triage later).

## Validation

| Check | Refusal |
|-------|---------|
| Target phase is `Define`, `Design`, or `Plan` | "Cannot bounce to <phase>. Valid: Define, Design, Plan." |
| Target phase is earlier than current phase in the flow | "Issue is at <current>; bouncing to <target> would advance, not regress. Use `/scope` to advance." |
| Issue is not already at `Phase=Bounced` | "Issue is already Bounced (BounceTo=<x>). Resolve that bounce first, or re-scope." |
| Issue is not at `Phase=Done` | "Issue is Done; bouncing a closed issue is a no-op. File a fresh issue if work needs to resume." |

Phase ordering for "is earlier" check:

```
Backlog (0) → Define (1) → Design (2) → Plan (3) → Ready (4) → Executing (5) → Refine (6) → Done (7)
```

A bounce from `Ready` to `Plan` is valid (target=3 < current=4). A bounce from `Plan` to `Plan` is invalid (no-op). A bounce from `Design` to `Plan` is invalid (advancing).

## Actions on pass

1. Set the project item's `Phase` field to `Bounced`, and reconcile the issue label to `phase:bounced` (see [Phase label reconcile](#phase-label-reconcile)).
2. Set the project item's `BounceTo` field to the target phase.
3. Post an audit comment:

   ```
   [bounce] Phase regression by <user>: <previous-phase> → Bounced (BounceTo=<target>).

   Reason: <text>
   ```

4. Print summary:

   ```
   ✓ #N bounced.
   Phase: <previous> → Bounced
   BounceTo: <target>
   Next: address the reason in the issue body or comments, then /scope #N
         (or /scope #N --phase <target> to force redo of that section)
   ```

## Resume contract with `/scope`

When `/scope <issue>` runs on a Bounced issue, it must:

1. Read `BounceTo` from the project item.
2. Set `Phase=<BounceTo>` (clears the Bounced state), reconciling the label from `phase:bounced` to `phase:<BounceTo>`.
3. Run from that phase forward — redoing the bounced phase and every downstream phase.

If the user passes `/scope <issue> --phase <name>` while bounced and `<name>` matches `BounceTo`, behavior is identical (the flag is the explicit form of the same intent). If `<name>` differs from `BounceTo`, honor `<name>` (the user is overriding) but post a comment noting the override.

## Self-bounce by other skills

Skills that detect their own wall conditions (`/scope` hitting a vague issue body, `/implement` discovering a broken assumption) call into the same logic: set `Phase=Bounced`, set `BounceTo=<phase>`, post a comment. The audit comment prefix changes to identify the source:

```
[scope] Self-bounce: <previous> → Bounced (BounceTo=Design).
   Question: <the blocker>

[implement] Self-bounce: Executing → Bounced (BounceTo=Plan).
   Discovered during implementation: <the issue>
```

Same skill mechanism, different invocation site. `/bounce` is the user-driven variant.

## Stalled (separate semantic)

`Phase=Stalled` is a different signal — env/tooling failure, not a phase regression. Examples: qodana CI service down, GitHub API rate-limited mid-batch, `gh` token expired. The issue's scoping is fine; the *environment* is the problem.

v1 has no `/stall` skill — set Stalled manually in the UI or via `gh project item-edit` with the BounceTo field left null. When you do, also set the `phase:stalled` label on the issue (`gh issue edit <n> --remove-label "phase:define,…,phase:bounced,phase:stalled" --add-label phase:stalled`) so the halt is visible in `gh issue list`. Future `/stall <issue> --reason "<env-issue>"` would post the same kind of audit comment.

## Phase label reconcile

The board `Phase` field is only visible on the project board — not on the issue itself or in `gh issue list`. This skill mirrors every Phase write as a `phase:*` label on the issue so the lifecycle is legible at a glance, and the label never disagrees with the board. **In the same step you set the `Phase` field, reconcile the label:**

```bash
gh issue edit <n> \
  --remove-label "phase:define,phase:design,phase:plan,phase:ready,phase:executing,phase:refine,phase:bounced,phase:stalled" \
  --add-label "phase:<new>"
```

`--remove-label` ignores labels the issue doesn't carry, so this single line is safe on any transition — it strips whatever phase label was present and applies the new one (lowercased: `Phase=Ready` → `phase:ready`). This skill writes `Phase=Bounced` (`phase:bounced`), and on the `/scope` resume contract `Phase=<BounceTo>` (`phase:<BounceTo>`).

## Failure modes

- **`release-state.json` stale**: rebuild via `/release-init <version> --reuse <num>`.
- **GitHub API failure mid-transition**: don't write a partial state. If the field-edit succeeds but the comment fails, retry the comment with backoff. If the field-edit fails, abort without retrying the comment.
- **Rate limits**: retry with backoff.

## What `/bounce` does NOT do

- Edit the issue body. The bounce reason is in a comment, not the body. `/scope` is the only skill that touches body sections.
- Decide *what* needs fixing. The `--reason` text is the user's framing; the skill records it verbatim. If the issue body needs new information for the bounce to be addressable, the user adds it (in a comment or body edit) before re-invoking `/scope`.
- Resume the issue. The user re-invokes `/scope` (or the orchestrator picks it up) once the reason is addressed.
- Cascade to dependent issues. If a bounced issue blocks other Plan/Ready issues, the user is responsible for triaging them too. A future skill could auto-bounce-cascade, but v1 keeps it manual.
