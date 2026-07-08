---
name: sweep
description: "Reclaim stale Aether local or workflow state. Use for worktree, branch, memory, ADR, or fat-issue sweeps with enumerate, classify, confirm, act, and report."
---

# Sweep

Use this Codex skill for repository cleanup sweeps.

## Source

- Workflow source: `.claude/skills/sweep/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Enumerate candidates, classify each by the target's stale signal, print a plan, and wait for confirmation before destructive or state-changing actions.
3. Use REST GitHub queries where the source requires them.
4. Never remove dirty, live, open-PR-attached, or ambiguous work without explicit confirmation.
5. Keep the target-specific semantics from the source for `worktrees`, `branches`, `memory`, `adrs`, `fat`, and `all`.
