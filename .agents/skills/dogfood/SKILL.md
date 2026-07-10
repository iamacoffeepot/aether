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
6. Persist and post: stage the run's evidence, commit it to the orphan `dogfood/evidence` branch, and upsert the rollup comment on the feature issue (see Persist and Post).

## Finding Categories

- `papercut`: awkward composition, surprising default, boilerplate, or rough error.
- `missing-primitive`: the consumer reached for a capability the engine lacks.
- `doc-gap`: the consumer could not proceed from docs and public signatures.
- `blocker`: the task stopped.

Soft-hold on a wrong artifact verdict or any high-severity blocker.

## Persist and Post

The workflow never touches GitHub. This skill makes the run durable after the rollup returns: it persists the evidence to the `dogfood/evidence` branch and posts the rollup as one living issue comment.

Stage a run directory holding this run's evidence:

- `rollup.json`: the workflow return written verbatim. The poster accepts the full `{rollup, task}` object or a bare rollup.
- `frame.png`: the judged frame, present only when a vision judge ran. The judge captures it to disk through `capture_frame` `save_path`, so the still is a file on the host rather than an image confined to the judge's context.
- `solution/`: the Attempt's scratch crate, for an `author` or `build-layer` medium. A `drive` medium writes no crate, so this is absent.

Name the run by a UTC timestamp in `YYYYMMDD-HHMMSS` form (`date -u +%Y%m%d-%H%M%S`). With the issue number it forms the `evidence/<issue>/<run-id>/` branch path and the `<issue>/<run-id>` run ref.

Commit the evidence, then post the rollup:

```
scripts/dogfood-evidence.sh <staged-run-dir> <issue> <run-id>

ISSUE=<issue> PR=<pr-if-any> RUN_REF=<issue>/<run-id> \
  ROLLUP_PATH=<staged-run-dir>/rollup.json HAS_FRAME=<1-when-frame-staged> \
  node scripts/post-dogfood-rollup.mjs
```

`scripts/dogfood-evidence.sh` does its git work in a throwaway worktree, bootstraps the orphan branch on first run, and retries a push rejected by a concurrent run. `scripts/post-dogfood-rollup.mjs` (node >= 20, no npm deps) upserts the marker-anchored comment with verdicts, friction grouped by category, soft-holds, the judged frame inline, and a viewer link, then sets `dogfood:unresolved` when the trial is actionable (attempt did not succeed, artifact verdict `wrong`, any soft-hold, or any `blocker` / `missing-primitive` friction) and clears it otherwise. Pass `PR` when a PR is open, since that is where `/land` reads the label; omit it to carry the label on the issue.
