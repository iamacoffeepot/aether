---
name: approve
description: "Validate an Aether scoped issue at Plan and advance it to Ready. Use after the user approves scope artifacts and before implementation dispatch."
---

# Approve

Use this Codex skill for the repository's Plan-to-Ready gate.

## Source

- Workflow source: `.claude/skills/approve/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Validate all gate checks from the source: phase, scoped sections, ADR references, model label, blocked labels, freshness, dependencies, and umbrella integrity.
3. Treat intended new files in a plan as creations, not removed existing targets, when applying the freshness gate.
4. If all gates pass and the user has approved, swap the issue to `phase:ready` with REST labels.
5. Do not dispatch implementation or edit the issue body.
