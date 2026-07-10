---
name: dogfood
description: Consumer-viewpoint validation of a landed feature — a fresh agent that never sees the implementation is handed a realistic task that exercises the new surface, accomplishes it through the public API only, and is graded on the friction it hit plus use-visible correctness (a vision judge over the rendered artifact). The complement to /review: review audits the producer's artifact, dogfood is a consumer-use trial that catches what only use reveals — ergonomic friction, missing primitives, awkward composition, surprising defaults, doc gaps. Use after a feature lands (or at the end of /implement, before un-draft) to trial it from the consumer's side. The Claude entry point to the dogfood workflow; the mirror of the Codex /dogfood skill.
---

# /dogfood — consumer-use trial of a landed feature

The Claude entry point to the `dogfood` workflow (`.claude/workflows/dogfood.js`). Where the workflow holds the multi-agent orchestration — Author → Attempt → Judge → Rollup — this skill is the thin invocation surface around it: it resolves the caller-side inputs the workflow sandbox cannot run itself, brings the live MCP harness up when a medium needs it, handles the heavy-medium approval stop-and-resume, and reports the returned rollup. The mirror of the Codex wrapper at `.agents/skills/dogfood/SKILL.md`, invoking the same workflow through the `Workflow` tool.

The `dogfood` workflow never touches GitHub and never gates CI. This skill is where the run becomes durable: once the rollup returns, the skill persists the run's evidence to the `dogfood/evidence` branch and posts the rollup as one living comment on the feature issue (see [Persist and post](#persist-and-post)). Filing the friction findings as follow-up issues stays a separate `/sketch` step.

## Invocation

```
/dogfood <issue>                          trial the landed feature scoped by <issue>
/dogfood <issue> --medium <m>             force the consumer medium: drive | author | build-layer
/dogfood <issue> --task "<task text>"     pass a pre-approved task, skipping the Author phase
```

The medium names what the consumer must write to exercise the surface:

- **drive** — nothing; drive the running engine over MCP. Runs straight through (no approval gate).
- **author** — a guest wasm component against the `aether-actor` SDK.
- **build-layer** — a new native cap / kind family / infra API on the workspace crates.

`author` and `build-layer` scaffold scratch crates and cost real compute, so the workflow stops for human approval on those before the Attempt runs (see [Approval gate](#approval-gate)). Omit `--medium` to let the Author phase pick the medium from the surface.

## Inputs

The skill assembles the workflow's arg contract (`{issue, diff, surface, task?, medium?}`, grounded against `.claude/workflows/dogfood.js`):

- `issue` — the landed feature's issue or scope text. Required unless `task` is supplied.
- `diff` — the landed diff. The Author phase may read it to frame the task; the Attempt agent never receives it (a consumer trial must not see the implementation).
- `surface` — public surface pointers: guide paths, crates, mail kinds, MCP tools, SDK macros.
- `task` — a pre-approved task object (`medium` / `prompt` / `surfaceUnderTest` / `expectedArtifact`) that skips the Author phase. This is what a `--task` flag or an approval resume supplies.
- `medium` — an optional forced medium (`drive` / `author` / `build-layer`).

## Caller-side prep

The workflow sandbox cannot run `git` or `grep`, so the skill resolves the inputs the workflow needs before invoking it:

1. **Resolve the issue text** — read the issue body (`gh api repos/iamacoffeepot/aether/issues/<n> --jq '.body'`) as `issue`. If the issue carries a `## Dogfood brief` section (emitted by `/scope`), lift it into a `task` directly — its `medium` / `prompt` / `surfaceUnderTest` / `expectedArtifact` fields are the workflow's task object verbatim, so a filled brief skips the Author phase and its approval gate. An `N/A` brief means the issue has no consumer surface to trial — say so and stop.
2. **Resolve the landed diff** — the merged change for the feature (`git show <merge-sha>` or the PR diff), passed as `diff` for the Author only.
3. **Resolve surface pointers** — the guide pages, crates, and mail kinds / MCP tools the feature introduced, from the issue's §Affected surfaces.

**Harness precondition.** The `drive` and `author` media exercise a running engine, so bring the live MCP harness up before invoking — run `scripts/ensure-tunnel.sh` (idempotent; a no-op if `:8890` is already bound, otherwise it launches the tunnel → hub → fleet detached). If the `mcp__aether-hub__*` tools are missing after a mid-session start, run `/mcp` to reconnect. A `build-layer` trial that only builds a scratch crate needs no harness.

## Workflow handoff

Invoke the workflow with the assembled args:

```
Workflow({name: "dogfood", args: {issue, diff, surface, task, medium}})
```

For a `drive` task (or any run where `task` is already in hand from a `--task` flag or a filled brief), the workflow runs straight through and returns the rollup.

### Approval gate

When the Author phase authors a fresh `author` or `build-layer` task, the workflow **stops after Author** and returns `{proposedTask, needsApproval: true, rollup: null}` rather than spending scratch-crate compute unbidden. On that return:

1. Surface `proposedTask` to the user — its `medium`, `prompt`, `surfaceUnderTest`, and `expectedArtifact`.
2. **STOP and wait.** Do not auto-approve — the whole point of the gate is that a heavy medium spends real compute, so a human confirms or edits the task first.
3. Re-invoke the workflow with `args.task` set to the approved (or user-edited) task: `Workflow({name: "dogfood", args: {issue, diff, surface, task: <approved>}})`. With `task` supplied the workflow skips Author and runs the Attempt → Judge → Rollup tail.

A `drive` task never returns `needsApproval` — it runs in a single call.

## Rollup report

On completion the workflow returns `{rollup, task}`. Report the rollup to the user:

- **totals / succeeded** — did the consumer accomplish the task through the public surface.
- **buildGreen** — for `author` / `build-layer`, did the scratch crate build (and any tests pass); `null` for `drive`.
- **artifact verdict** — for a task that renders, the vision judge's `correct` / `wrong` / `n-a` over the captured frame, with the visual discrepancy on a `wrong`.
- **friction by category** — the logged friction grouped `papercut` / `missing-primitive` / `doc-gap` / `blocker`, each with where it hit, what it was, and a suggested consumer-side fix.
- **soft-holds** — a `wrong` artifact verdict or any high-severity `blocker`. Advisory only — `/dogfood` never blocks a land; the soft-hold is a flag for the reviewer.

## Persist and post

The rollup that the workflow returns dies with the conversation, and the judge's captured frame lives only inside the judge agent's vision context. This tail makes the run durable: it stages the evidence into a run directory, commits it to the orphan `dogfood/evidence` branch, and posts the rollup onto the feature issue.

**Stage the run directory.** Assemble one directory holding this run's evidence:

- `rollup.json` — the workflow return written verbatim (`{rollup, task}`; the poster accepts the full object or a bare rollup).
- `frame.png` — the judged frame, present only when a vision judge ran. The judge captures it to disk through `capture_frame`'s `save_path` (ADR-referenced #2962), so the still is a file on the harness host rather than an image that lived only in the judge's context.
- `solution/` — the Attempt's scratch crate, for an `author` or `build-layer` medium. Copy the crate the Attempt authored so a later reader can see what the consumer actually built. A `drive` medium writes no crate, so `solution/` is absent.

**Name the run.** The run id is a UTC timestamp in `YYYYMMDD-HHMMSS` form (`date -u +%Y%m%d-%H%M%S`). Paired with the issue number it forms the `evidence/<issue>/<run-id>/` path on the branch and the `<issue>/<run-id>` run ref the poster and viewer share.

**Commit the evidence.** Push the staged directory to the branch:

```
scripts/dogfood-evidence.sh <staged-run-dir> <issue> <run-id>
```

The script does all its git work in a throwaway worktree — the invoking checkout is never touched — bootstrapping the orphan branch on first run and retrying a rejected push against a concurrent run.

**Post the rollup.** Upsert the living comment and set or clear the advisory label:

```
ISSUE=<issue> PR=<pr-if-any> RUN_REF=<issue>/<run-id> \
  ROLLUP_PATH=<staged-run-dir>/rollup.json HAS_FRAME=<1-when-frame-staged> \
  node scripts/post-dogfood-rollup.mjs
```

The comment renders the verdicts, the friction grouped by category, the soft-holds, the judged frame inline (from its evidence-branch raw URL, when a frame was staged), and a viewer link to the full run. `dogfood:unresolved` is set when the trial is actionable — the attempt did not succeed, the artifact verdict is `wrong`, any soft-hold fired, or any friction is a `blocker` or `missing-primitive` — and cleared on a clean re-run. Pass `PR` when a PR is open, since that is where `/land` reads the label; omit it to carry the label on the issue.

## What `/dogfood` does NOT do

- File follow-up issues. The rollup's friction findings are triaged into `papercut` / missing-primitive / doc-gap issues via a separate `/sketch` step — the user's call which.
- Gate CI or hard-block a land. The persist-and-post tail writes the evidence branch and the rollup comment, and `dogfood:unresolved` flags an actionable trial for the `/land` gate, but the trial itself never fails CI and never blocks a merge on its own.
- Show the Attempt agent the implementation. The diff and implementation files go to the Author phase only — a consumer trial that sees the code is not a consumer trial.
- Reimplement the orchestration. The `dogfood` workflow is the single source of the Author → Attempt → Judge → Rollup logic; this skill only invokes it.
