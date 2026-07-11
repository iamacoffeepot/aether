---
name: implement
description: "Implement an approved Aether issue in a separate worktree, open a draft PR, and drive CI to green. Use only for phase:ready issues unless the user explicitly requests quick mode."
---

# Implement

Use this Codex skill for the repository's issue-to-draft-PR workflow.

## Source

- Workflow source: `.claude/skills/implement/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Verify preconditions: `phase:ready`, exactly one `model:*`, no umbrella sub-issues, an implementation plan, and working `gh` auth with `repo` scope.
3. Create a dedicated Codex worktree under `.agents/worktrees/issue-<N>` from `origin/main`; do not edit the primary `main` checkout.
4. Move the issue to `phase:executing` before implementation work starts.
5. Follow the issue's `## Implementation plan` literally. Deviations are bounces, not freelancing.
6. Commit the work, then run the cheap deterministic pre-PR tier before any push that opens or updates a draft PR: `cargo fmt -- --check` and `cargo clippy --all-targets -- -D warnings`. If either check fails, fix it locally and amend or recommit before pushing; do not push a known fmt, compile, or clippy red. Assert a clean worktree, push, and open a draft PR over REST.
7. Use `scripts/wave-status.sh --wait <pr>` for CI monitoring. Leave successful work at `phase:refine` with the PR still draft.

## Model-routed hybrid dispatch

Apply this section whenever the Claude source calls for a background implementation agent. Keep a single in-session implementation in the parent; it does not need a child model route.

1. Read the issue labels over REST and require exactly one route:

   | Label | Named agent | Exact model |
   |---|---|---|
   | `model:haiku` | `luna` | `gpt-5.6-luna` |
   | `model:sonnet` | `terra` | `gpt-5.6-terra` |
   | `model:opus` | `sol` | `gpt-5.6-sol` |
   | `model:fable` | `sol` | `gpt-5.6-sol` |

   Refuse a missing, duplicate, or unknown route. Do not fall back to the parent model.
2. Prefer the project agents in `.codex/agents/` only when the active subagent tool exposes an explicit agent-type or model selector. Select the named agent in the table in the tool call itself; prompt wording or a task name is not proof of model selection.
3. When the active subagent tool has no selector, launch the worker in its issue worktree with the exact model:

   ```text
   codex exec --model <exact-model> --json --sandbox workspace-write \
     --config 'approval_policy="never"' --cd <issue-worktree> -
   ```

   Send the bounded worker prompt on standard input. Do not pass `--ephemeral`: retain the `thread.started` session id from the JSONL stream so a focused correction can use `codex exec resume --model <exact-model> <session-id> <prompt>`. If the model is unavailable or the process cannot start, fail the dispatch instead of retrying on another model.
4. Bound the worker prompt to the hybrid handoff from the source skill: follow the scoped implementation plan, change only the issue worktree, run the assigned local verification, commit all intended changes, assert a clean tree, and stop. Forbid phase-label edits, pushes, PR creation, CI/review handling, merges, worktree removal, and repository-side scratch files. Keep temporary orchestration output outside the repository and remove it after the handoff.
5. The parent remains responsible for the `phase:executing` transition before dispatch and for reviewing the committed diff, pushing, opening the draft PR, addressing CI/review results, and leaving the issue at `phase:refine` when green.
