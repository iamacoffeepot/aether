---
name: dogfood
description: "Run Aether consumer-viewpoint dogfood validation. Use after a feature lands or before un-drafting a PR to have fresh agents consume the public surface, log friction, and optionally judge rendered output."
---

# Dogfood

Use this Codex skill for the consumer-use trial described by `.claude/workflows/dogfood.js`.

## Source

- Workflow source: `.claude/workflows/dogfood.js`
- Translation rules: `../_shared/claude-to-codex.md`

Read both before running the workflow.

## Inputs

Accept these values from the user or caller:

- `issue`: landed feature issue or scope text. Required unless `task` is supplied.
- `diff`: landed diff. Author may read it; Attempt must never receive it.
- `surface`: public surface pointers, such as guide paths, crates, mail kinds, or MCP tools.
- `task`: approved task object that skips Author.
- `medium`: optional forced medium: `drive`, `author`, or `build-layer`.

## Phases

1. Author: create or validate a realistic task with `medium`, `prompt`, `surfaceUnderTest`, and `expectedArtifact`. Use a subagent when the task needs independent framing. The Author may see `issue`, `diff`, and `surface`.
2. Approval gate: if the authored medium is `author` or `build-layer`, stop and show the proposed task for human approval. Continue only when the user supplies or approves the task.
3. Attempt: spawn a fresh Codex subagent without forked context. Pass only the approved task and public surface pointers. Do not pass the diff, implementation files, or Author reasoning. Require output with `succeeded`, `summary`, `engineId`, `buildGreen`, and `findings`.
4. Judge: if `expectedArtifact` is non-null and Attempt leaves an `engineId`, spawn a fresh judge subagent to capture and inspect the frame through the Aether MCP tools, then terminate the substrate.
5. Rollup: summarize totals, grouped friction, artifact verdict, and soft holds. Do not file follow-up issues from this skill.

## Finding Categories

- `papercut`: awkward composition, surprising default, boilerplate, or rough error.
- `missing-primitive`: the consumer reached for a capability the engine lacks.
- `doc-gap`: the consumer could not proceed from docs and public signatures.
- `blocker`: the task stopped.

Soft-hold on a wrong artifact verdict or any high-severity blocker.
