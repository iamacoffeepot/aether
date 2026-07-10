---
name: dogfood
description: Consumer-viewpoint validation of a landed feature — a fresh agent that never sees the implementation is handed a realistic task that exercises the new surface, accomplishes it through the public API only, and is graded on the friction it hit plus use-visible correctness (a vision judge over the rendered artifact). The complement to /review: review audits the producer's artifact, dogfood is a consumer-use trial that catches what only use reveals — ergonomic friction, missing primitives, awkward composition, surprising defaults, doc gaps. Use after a feature lands (or at the end of /implement, before un-draft) to trial it from the consumer's side. The Claude entry point to the dogfood workflow; the mirror of the Codex /dogfood skill.
---

# /dogfood — consumer-use trial of a landed feature

The Claude entry point to the `dogfood` workflow (`.claude/workflows/dogfood.js`). Where the workflow holds the multi-agent orchestration — Author → Attempt → Judge → Rollup — this skill is the thin invocation surface around it: it resolves the caller-side inputs the workflow sandbox cannot run itself, brings the live MCP harness up when a medium needs it, handles the heavy-medium approval stop-and-resume, and reports the returned rollup. The mirror of the Codex wrapper at `.agents/skills/dogfood/SKILL.md`, invoking the same workflow through the `Workflow` tool.

`/dogfood` never touches GitHub, never gates CI, and files nothing — surfacing the friction it finds as follow-up issues is a separate `/sketch` step.

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

## What `/dogfood` does NOT do

- File follow-up issues. The rollup's friction findings are triaged into `papercut` / missing-primitive / doc-gap issues via a separate `/sketch` step — the user's call which.
- Touch GitHub or gate CI. The trial is advisory; the rollup is the surface.
- Show the Attempt agent the implementation. The diff and implementation files go to the Author phase only — a consumer trial that sees the code is not a consumer trial.
- Reimplement the orchestration. The `dogfood` workflow is the single source of the Author → Attempt → Judge → Rollup logic; this skill only invokes it.
