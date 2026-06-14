---
name: bounce
description: Explicit phase regression. Move an issue from its current Phase back to an earlier one (Plan / Design / Define), record the reason as a comment, and set a bounce-to:* label so /scope can resume from the right place. Required reason — no silent bounces. For env/tooling halts, use Phase=Stalled manually (no skill in v1).
---

# /bounce — explicit phase regression

The user invokes `/bounce` when reviewing scope artifacts (or watching execution) and concludes an upstream phase needs rework. The skill records the regression as a `phase:bounced` label plus a `bounce-to:<phase>` label, with the reason posted as a comment. `/scope` then resumes from the target phase on next invocation.

Self-bounces by other skills (`/scope` hitting a wall, `/implement` discovering a design flaw mid-execution) use the same mechanism — this skill is the explicit user-driven wrapper.

## Invocation

```
/bounce <issue> <to-phase> --reason "<text>"
```

`<to-phase>` is one of: `Define`, `Design`, `Plan`. `--reason` is required.

## Preconditions

1. `--reason` is non-empty (no silent bounces — the corpus shows bounces without context are the hardest to triage later).

## Validation

| Check | Refusal |
|-------|---------|
| Target phase is `Define`, `Design`, or `Plan` | "Cannot bounce to <phase>. Valid: Define, Design, Plan." |
| Target phase is earlier than current phase in the flow | "Issue is at <current>; bouncing to <target> would advance, not regress. Use `/scope` to advance." |
| Issue is not already at `Phase=Bounced` | "Issue is already Bounced (bounce-to:<x>). Resolve that bounce first, or re-scope." |
| Issue is not at `Phase=Done` | "Issue is Done; bouncing a closed issue is a no-op. File a fresh issue if work needs to resume." |

Phase ordering for "is earlier" check:

```
Backlog (0) → Define (1) → Design (2) → Plan (3) → Ready (4) → Executing (5) → Refine (6) → Done (7)
```

A bounce from `Ready` to `Plan` is valid (target=3 < current=4). A bounce from `Plan` to `Plan` is invalid (no-op). A bounce from `Design` to `Plan` is invalid (advancing).

## Actions on pass

1. Over REST, reconcile the issue label to `phase:bounced` and stamp `bounce-to:<target>` (replacing any prior `bounce-to:*`) — both the bounced state and the resume phase live on labels (see [Phase label reconcile](#phase-label-reconcile)).
2. Post the reason as a comment — it is the surviving comment class (information addressed to a human with no structured home), written as prose markdown with a bold lead:

   ```markdown
   **Bounced to <target>** (from <previous-phase>)

   <reason text, verbatim>
   ```

3. Print summary:

   ```
   ✓ #N bounced.
   Phase: <previous> → Bounced
   bounce-to: <target>
   Next: address the reason in the issue body or comments, then /scope #N
         (or /scope #N --phase <target> to force redo of that section)
   ```

## Resume contract with `/scope`

When `/scope <issue>` runs on a Bounced issue, it must:

1. Read the `bounce-to:*` label from the issue (REST).
2. Set `Phase=<target>` (clears the Bounced state), reconciling the label from `phase:bounced` to `phase:<target>` and removing the now-consumed `bounce-to:*` label in the same REST `PUT`.
3. Run from that phase forward — redoing the bounced phase and every downstream phase.

If the user passes `/scope <issue> --phase <name>` while bounced and `<name>` matches the `bounce-to:*` label, behavior is identical (the flag is the explicit form of the same intent). If `<name>` differs from the label, honor `<name>` (the user is overriding) and note the override in the run's output.

## Self-bounce by other skills

Skills that detect their own wall conditions (`/scope` hitting a vague issue body, `/implement` discovering a broken assumption) call into the same logic: set `Phase=Bounced`, stamp the `bounce-to:<phase>` label, post the blocker as a comment. Same prose-markdown shape, with the lead naming what's blocked and the body carrying the question or finding:

```markdown
**Bounced to Design** — the two candidate shapes are genuinely tied.

<the specific question the user must answer>
```

```markdown
**Bounced to Plan** — discovered during implementation.

<the broken assumption, with the file/test that exposed it; for /implement,
the attempt history follows here>
```

Same skill mechanism, different invocation site. `/bounce` is the user-driven variant.

## Stalled (separate semantic)

`Phase=Stalled` is a different signal — env/tooling failure, not a phase regression. Examples: qodana CI service down, GitHub API rate-limited mid-batch, `gh` token expired. The issue's scoping is fine; the *environment* is the problem.

v1 has no `/stall` skill — set the `phase:stalled` label manually, with no `bounce-to:*` label (a stall is not a phase regression). Use the REST swap from [Phase label reconcile](#phase-label-reconcile), with `new="phase:stalled"`, so the halt is visible on the issue. Future `/stall <issue> --reason "<env-issue>"` would post the reason the same way `/bounce` does — it's the same surviving comment class.

## Phase label reconcile

The `phase:*` label is the canonical phase state — the only phase store the pipeline keeps, legible on the issue itself and discoverable over the REST issues endpoint. The swap rides REST: `gh issue edit --add-label/--remove-label` is GraphQL-backed, while the `gh api …/labels` endpoints are REST, so the phase write stays off the contended pool.

```bash
# Atomic swap to an active phase. Runs under bash for array word-splitting.
bash <<'EOF'
n=<n>; new="phase:<new>"; repo=iamacoffeepot/aether
args=()
while IFS= read -r l; do args+=(-f "labels[]=$l"); done < <(
  gh api "repos/$repo/issues/$n/labels" --jq '.[].name | select(startswith("phase:") | not)')
args+=(-f "labels[]=$new")
gh api -X PUT "repos/$repo/issues/$n/labels" "${args[@]}"
EOF
```

The single `PUT …/labels` replaces the label set with the non-`phase:*` labels plus the one new `phase:*`, so the issue never carries two phase labels and never carries zero — a tighter guarantee than a remove-then-add pair, which has a window between its two calls. A failed PUT leaves the prior labels untouched and heals on the next run. This skill writes `phase:bounced` plus a `bounce-to:<target>` label; on the `/scope` resume contract that becomes `phase:<target>` with the `bounce-to:*` label cleared. The three `bounce-to:plan|design|define` labels must exist in the repo (`gh label create` once) for the stamp to apply.

## Failure modes

- **GitHub API failure mid-transition**: don't write a partial state. If the label swap succeeds but the comment fails, retry the comment with backoff. If the label swap fails, abort without retrying the comment.
- **Rate limits**: retry with backoff.

## What `/bounce` does NOT do

- Edit the issue body. The bounce reason is in a comment, not the body. `/scope` is the only skill that touches body sections.
- Decide *what* needs fixing. The `--reason` text is the user's framing; the skill records it verbatim. If the issue body needs new information for the bounce to be addressable, the user adds it (in a comment or body edit) before re-invoking `/scope`.
- Resume the issue. The user re-invokes `/scope` (or the orchestrator picks it up) once the reason is addressed.
- Cascade to dependent issues. If a bounced issue blocks other Plan/Ready issues, the user is responsible for triaging them too. A future skill could auto-bounce-cascade, but v1 keeps it manual.
