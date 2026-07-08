---
name: sketch
description: "Capture an idea as a well-formed Aether GitHub issue. Use when filing Backlog issues from rough ideas while preserving the user's words, conventional title/labels, and no phase label."
---

# Sketch

Use this Codex skill for the repository's idea-to-issue workflow.

## Source

- Workflow source: `.claude/skills/sketch/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before filing.
2. Follow the Claude sketch mechanics, translated for Codex.
3. File issues over REST with `gh api -X POST repos/iamacoffeepot/aether/issues`.
4. Preserve the user's sketch verbatim in the `## Description` blockquote.
5. Apply `type:*` and crate labels inline on creation. Do not add a `phase:*` label.
6. Do not scope, design, plan, comment, or open a PR.
