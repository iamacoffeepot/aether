---
name: wish
description: "Run Aether adversity-grounded design ideation. Use to drill from a felt absence into producible plans, including deep-mode driller, skeptic, and synthesis workflows."
---

# Wish

Use this Codex skill for the repository's design ideation workflow.

## Source

- Skill source: `.claude/skills/wish/SKILL.md`
- Deep workflow source: `.claude/workflows/wish.js`
- Translation rules: `../_shared/claude-to-codex.md`

Read all relevant source files before acting. For normal wish work, read the skill source and translation rules. For `--deep`, also read the deep workflow source.

## Core Rules

1. Ground every claimed existing engine surface against current code with `rg`, `git grep`, or file reads. Do not rely on memory for kinds, capabilities, mailboxes, traits, file paths, or signatures.
2. A wish is not a plan until it is producible with known, verified means within current resources.
3. Do not file issues or write production code from this skill unless the user asks for a follow-up sketch/scope flow.
4. Preserve alternatives, doors opened, and doors closed in the wish output.

## Deep Mode

For `/wish --deep`, map the Claude JS workflow to Codex subagents:

1. Generate or accept root wishes from the user-facing wish skill flow.
2. Maintain the frontier in the main Codex thread.
3. Spawn fresh driller subagents for bounded nodes. Each driller writes its own `wish.md` and returns `producible`, `summary`, `grounded_surfaces`, and `children`.
4. For each `producible:true` node, run a skeptic pass before accepting it as terminal.
5. Synthesize an `index.md` from node summaries and terminal leaves.

Keep the main thread's context focused on frontier state and summaries, not raw driller reasoning.
