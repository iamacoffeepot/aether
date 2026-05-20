---
name: orchestrate
description: Run an entire approved release end-to-end. Reads the unit dependency graph in .claude/release-units.json, dispatches every ready unit in parallel as a worktree agent, watches their PRs merge under bounded auto-merge, promotes newly-unblocked units, and loops until the release is done or a unit bounces. The "Phase C orchestrator" the /implement skill anticipates. Invoke as `/orchestrate` (run), `/orchestrate plan` (build/refresh the manifest), or `/orchestrate status` (print the unit DAG state).
---

# /orchestrate — whole-release execution

The top of the release flow. Where `/scope` + `/approve` produce vetted per-issue plans and `/implement` runs *one* issue, `/orchestrate` runs the **whole release graph**: it consumes a declared dependency manifest, executes independent units in parallel, sequences dependent units behind their prerequisites, and merges under bounded authority — so the user approves the shapes once and the release runs unattended.

This is the "Phase C orchestrator" `/implement` refers to ("merges under bounded auth"). It reuses `/implement`'s execute⇄refine loop as the per-unit worker; its own job is purely the deterministic outer loop: *which units are ready, dispatch them, watch them merge, promote the next layer.*

## Why units, not issues

`/implement` branches each issue from `main`. A dependent issue (e.g. the DAG validator, which `use`s the wire kinds from a sibling issue) gets a tree without its prerequisite's code and can't compile. That partition is the thing this skill exists to solve, two ways at once:

- **A unit is a dependency chain built on one branch.** One agent implements the unit's issues *in order* on a single branch, so the shapes snap together in one build tree and the chain compiles end-to-end. The unit is an *atomic* execute-and-merge artifact: nothing in it merges until the whole unit's PR is green.
- **Cross-unit deps are satisfied by merge order.** The orchestrator only dispatches a unit after every unit in its `depends_on` has *merged to main*. So each unit branches from fresh `main` with its prerequisites' code already present — no stacking, no rebase cascades.

Independent units run fully in parallel. The only serialization is a real dependency edge.

## The manifest — `.claude/release-units.json`

The deterministic input. Sequencing is data, never the orchestrator's ad-hoc judgment. Schema:

```json
{
  "release_version": "0.4",
  "project_node_id": "PVT_...",
  "owner": "iamacoffeepot",
  "repo": "aether",
  "auth": { "mode": "bounded", "retry_cap": 3, "wall_clock_min": 45 },
  "units": [
    {
      "id": "dag-core",
      "title": "DAG submit/validate/execute/MCP + reliable settlement",
      "type": "feat",
      "branch": "feat/dag-core",
      "issues": [974, 975, 1031, 976, 977],
      "depends_on": []
    },
    { "id": "transforms", "issues": [979, 982, 1012], "depends_on": ["dag-core"], "...": "..." }
  ],
  "tracking_only": [973, 978, 983, 989],
  "excluded": { "960": "parked", "1017": "folded into dag-core" }
}
```

- `units[].issues` is the **ordered build sequence** within the unit (topologically sorted by intra-unit dependency).
- `units[].depends_on` lists unit ids that must be merged before this unit dispatches.
- `tracking_only` are umbrella issues — not executed; they close when their children do (manual / future `/release-promote-umbrella`).
- The manifest is gitignored operational state alongside `release-state.json`. It is release-specific; a new release gets a fresh one.

The graph in `units` + `depends_on` must be acyclic. `/orchestrate` validates this on load (a cycle is a manifest authoring error — abort with the offending edge set).

## Invocation

```
/orchestrate                 run the release: dispatch ready units, promote on merge, loop
/orchestrate plan            build/refresh .claude/release-units.json from the board (proposal; user edits)
/orchestrate status          print the unit DAG state (done / in-flight / ready / blocked / bounced)
/orchestrate <unit-id>       run a single named unit (manual, ignores the loop)
/orchestrate --dry-run       print the dispatch plan (which units would fire now) without dispatching
```

## Preconditions (run)

1. `.claude/release-state.json` exists (field cache). Else: *"Run `/release-init <version>` first."*
2. `.claude/release-units.json` exists. Else: *"No unit manifest. Run `/orchestrate plan` first."*
3. The manifest's `project_node_id` matches `release-state.json`'s. Else the manifest is stale for the active release — abort.
4. **Release-approval gate.** Every issue in every unit must be at `Phase=Ready, AgentReady=Yes`. If any are not:
   - List the unapproved/unscoped issues grouped by unit.
   - Do **not** auto-approve. Print: *"N issues are not approved. This is the release-approval gate — review them, then approve (`/approve <issue>` each, or confirm here to approve all listed) before the release runs."*
   - If the user confirms "approve all," flip each listed issue to `Phase=Ready, AgentReady=Yes` with an `[orchestrate] release-approval by <user>` audit comment, then proceed. This one confirmation **is** the "approve the shapes once" gate.

## The orchestration loop (deterministic)

The orchestrator (this session) runs a mechanical loop. It makes **no** sequencing decisions beyond reading the graph.

```
load manifest; validate acyclic; load board state
preflight (release-approval gate above)

unit_state := { every unit: "pending" }
mark units already merged (their PR merged / all issues Done) as "done"

loop:
  ready := units where state=="pending" AND every depends_on is "done"
  for each ready unit:
      dispatch it (background worktree agent — see "Per-unit worker"); state := "in-flight"
      audit-comment each of its issues: [orchestrate] dispatched in unit <id> (branch <branch>)
      set each of its issues Phase=Executing

  if no units are "in-flight" and no units are "ready":
      break        # nothing left to do (all done, or remainder blocked by a bounce)

  wait for the next event:
    - a unit agent reports completion (PR open + auto-merge queued, OR self-bounced)
    - poll in-flight units' PRs for merge   (gh pr view <pr> --json state,mergedAt)

  on a unit's PR MERGED:
      state := "done"; set its issues Phase=Done; audit-comment [orchestrate] unit <id> merged
      # loop recomputes `ready` — dependents may now unblock

  on a unit BOUNCED (agent self-bounced, or caps hit):
      state := "bounced"; surface to user with the blocking issue + reason
      # its dependents stay "pending" forever this run — do NOT dispatch them
      # independent units keep running

report:
  done units, bounced units (+ reason + which issue), units left blocked-by-a-bounce, any Stalled
```

Key points:

- **A unit agent completing ≠ its PR merged.** The agent finishes when the PR is open and auto-merge is *queued*; the actual merge happens GitHub-side after CI passes. **Dependents promote on the PR's MERGE, not the agent's completion.** So after an agent returns, poll its PR until `mergedAt` is non-null before treating the unit as `done`.
- **Polling cadence.** Independent units are dispatched immediately (no waiting). For dependent units, poll the prerequisite PRs. CI + auto-merge takes minutes; poll every ~270s (stays inside the prompt-cache window) via `ScheduleWakeup`, or run a `Monitor` until-loop on `gh pr view`. Do not busy-poll.
- **Parallelism.** Dispatch *all* currently-ready units in one pass (multiple background agents). For 0.4 the first pass fires `dag-core`, `store`, `caps`, `fix-flake-999`, `fix-963`, `fix-964` concurrently; `transforms` waits for `dag-core` to merge.
- **The orchestrator is a deterministic process, not a Claude agent making calls.** The graph decides order; this session executes it mechanically. Per-unit *work* is the Claude agent.

## Per-unit worker (the dispatched agent)

Each ready unit is dispatched as a `general-purpose` Agent under `isolation: "worktree"`, run in the background. The prompt is self-contained (the agent has no conversation context). Template:

> Implement release unit **`<unit-id>`** of the aether 0.4 release end-to-end, on a single branch. Repo: https://github.com/iamacoffeepot/aether
>
> The unit is this ordered list of GitHub issues — build them **in this order**, on **one branch**, so each compiles against the prior:
> `<issues, in order, each as iamacoffeepot/aether#N with its title>`
>
> Branch `<branch>` from **fresh `main`** (every unit this depends on has already merged, so its code is in `main`). For each issue in order:
>   1. Read the issue's **plan section** — `## Implementation plan`, or in the filing-agent/report format the equivalent header: `## Plan`, `## Proposed design`, or `## Suggested fix`. It carries the exact change. Follow it literally (same files, same sequence, same tests). Deviation is a bounce, not freelancing.
>   2. Implement it; commit with a Conventional Commits subject `<type>(scope): description (issue <N>)`, lowercase subject.
>   3. Run `cargo check` so the next issue builds on a compiling tree.
> After the last issue, run the full local pre-flight: `scripts/preflight.sh` (covers `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo doc --workspace --no-deps`, `cargo nextest run --workspace --profile ci`, and the wasm32 component cross-build). Do NOT run `cargo test --workspace` — use nextest (per `feedback_use_nextest_not_cargo_test.md`).
>
> Push the branch and open **one** PR titled `<type>(scope): <unit title>` with a body that closes every issue in the unit — `Closes iamacoffeepot/aether#N` on its own line per issue (cross-repo form is required; bare `#N` is stripped by the PR-body hook) — plus a short per-issue summary.
>
> Then run the CI refine loop: `gh pr checks <pr> --watch`; on failure pull `gh run view <run-id> --log-failed`, classify (format/clippy/doc/build = always real mechanical fix; same test twice = real; different test each time = flake, rerun without push; pre-existing test breaks = scope expansion, bounce), fix in the worktree, push, repeat. Caps: **<retry_cap>** real-failure fixes, **<wall_clock_min>** minutes wall-clock total.
>
> {{automerge_clause}}
>
> If you hit a cap, or discover the plan is wrong (approach doesn't work / needs changes outside the §Implementation plan), **self-bounce**: set the *specific blocking issue*'s project fields `Phase=Bounced`, `BounceTo=<Plan|Design>`, post `[orchestrate] unit <unit-id> self-bounce at issue <N>: <reason>` on that issue, and return reporting which issue blocked and why. Do NOT force the change. Issues earlier in the unit that you already built stay on the branch but do not merge (the unit's one PR is atomic — nothing merges until the whole unit is green).
>
> Before returning, unlock your worktree: `git worktree unlock "$(git rev-parse --show-toplevel)"` (best-effort). Hand back the PR URL, the merge state, and — if bounced — the blocking issue + reason.
>
> Constraints: no destructive git ops, no `--no-verify`, no force-push, no amend. Do not touch `.claude/` files. Branch from `main`, not any other branch.

`{{automerge_clause}}` is set from the manifest's `auth.mode`:

- **`bounded`:** `When CI is green, queue auto-merge: \`gh pr merge <pr> --auto --squash --delete-branch\`. The release has authorized bounded auto-merge for this unit. The merge fires GitHub-side once CI passes and required reviews are satisfied; you do not wait for the merge itself — returning with auto-merge queued is success.`
- (Future modes — `roots-only`, `manual` — set the clause to "do not self-merge; return with the PR green and open.")

## Bounded authority (the #936 lesson)

Approving the release authorizes units to **self-merge as CI goes green, within bounds** — it is not blanket automerge:

- **Per-unit caps** — `retry_cap` real-failure fixes and `wall_clock_min` total. Hitting either self-bounces the unit (to Plan) rather than grinding. Recorded in each issue's `AuthBudget` field.
- **Bounce stops the chain.** A bounced unit's dependents are never dispatched this run. The orchestrator surfaces the bounce and keeps independent units going; it does **not** auto-retry, re-scope, or route around a bounce. Resuming is a human decision (fix the plan, re-`/approve`, re-run `/orchestrate`).
- **Anything unexpected halts that unit.** Stalled (env/tooling failure — qodana down, rate limit, network) → mark the unit's issues `Phase=Stalled`, abort that unit's loop, surface. Phase regression detected mid-flight (someone hand-bounced an issue) → abort that unit, leave branch/PR as-is.
- **No surprise blast radius.** Only units whose issues are all `AgentReady=Yes` ever dispatch. The manifest's `excluded`/`tracking_only` sets never execute.

## `/orchestrate plan`

Proposes a manifest from the active board so the user isn't hand-authoring JSON from scratch:

1. List the project's open, non-`tracking_only` issues.
2. Infer dependency edges from issue bodies: a `Part of <umbrella>` line groups an issue under a phase; explicit `Blocked by <owner/repo#N>` / `depends on #N` / "needs <type> from #N" lines become edges; ADR-declared sequencing (e.g. "the executor arm is gated on #1031") becomes an edge.
3. Group maximal dependency chains into units; leave independent issues as single-issue units.
4. Topologically order each unit's `issues` and the inter-unit `depends_on`.
5. Write the proposal to `.claude/release-units.json` and **print it for the user to review and edit** — inference is a starting point, not authoritative. The user owns the final graph.

`plan` never dispatches. It only writes the manifest.

## `/orchestrate status`

Reads the manifest + board and prints the unit DAG: for each unit, its state (done / in-flight: PR #N / ready / blocked-by: `<unit>` / bounced: issue #N), and the critical path. Read-only; no dispatch, no field writes.

## Failure modes

- **Manifest cycle** — abort on load, name the offending `depends_on` edges.
- **Manifest references an issue not on the board** — abort, name it; the user adds it or fixes the manifest.
- **`release-state.json` field cache stale** — same as the other skills: `/release-init <version> --reuse <num>` to rebuild.
- **A unit agent's worktree path collides** (stale `/tmp/...` from an aborted run) — the agent removes the stale worktree and retries; if `git worktree list` is wedged, surface to the user.
- **`gh` rate-limited mid-loop** — back off; if persistent, mark in-flight units' state as unknown, stop dispatching new units, surface the reset time. Do not lose track of what's already in flight.
- **Two units edit the same file** — the manifest author's responsibility to avoid (units should be crate/area-disjoint, or sequenced via `depends_on`). If two in-flight units conflict at merge, the second's auto-merge fails CI/merge → that unit's refine loop sees it → bounce. Surfaces, doesn't corrupt.
- **Orchestrator session interrupted** — re-running `/orchestrate` recomputes state from the board (merged PRs → done; open green PRs → in-flight; bounced → bounced), so it resumes without double-dispatching. State lives on GitHub, not in the session.

## What `/orchestrate` does NOT do

- Author the dependency graph from nothing — `/orchestrate plan` proposes, the user owns the final manifest.
- Edit issue bodies — `/scope` only.
- Auto-retry, re-scope, or route around a bounced unit. Bounces are human decisions.
- Merge outside the manifest's `auth` bounds, or dispatch any issue not `AgentReady=Yes`.
- Run umbrella (`tracking_only`) issues, or `excluded` ones.
- Clean up worktrees — `/sweep` reclaims them after PRs merge.
- Notify anyone. Audit comments + the run report are the surface.

## Relationship to the other skills

| Skill | Scope | This skill's use of it |
|-------|-------|------------------------|
| `/release-init` | bootstraps the project + field cache | prerequisite; provides `release-state.json` |
| `/scope` | one issue → vetted plan | prerequisite per issue; `plan`'s inference reads its output |
| `/approve` | Plan → Ready, the human gate | the release-approval gate batches this across the manifest |
| `/implement` | one issue, execute⇄refine, stops at green | the per-unit worker is `/implement`'s loop generalized to an ordered issue list + bounded auto-merge |
| `/bounce` | explicit phase regression | the self-bounce mechanism a unit agent uses on a cap/design wall |
| `/sweep` | reclaim merged worktrees | run after a release completes to clean up |
