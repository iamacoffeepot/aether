---
name: implement
description: Execute an approved release issue end-to-end. Cuts a branch in an isolated worktree, implements per the issue's §Implementation plan, opens a PR, then loops on CI feedback until green or self-bounce. Does NOT merge — stops at "PR open, CI green" awaiting user or orchestrator. Requires Phase=Ready, AgentReady=Yes.
---

# /implement — release-flow execution skill

The execution side of the release flow. Pairs with `/scope` and `/approve`: where those produce a vetted plan, `/implement` carries it out. Loops the Execute⇄Refine cycle internally until CI is green, then stops and waits for merge (does *not* auto-merge — `/delegate` does, this doesn't).

Distinct from `/delegate`. `/delegate` is for ad-hoc agent-labeled work outside the Project flow; `/implement` is purpose-built for issues that have passed `/approve`. Both share worktree isolation; everything else differs.

## Invocation

```
/implement <issue>                       run with defaults (retry-cap=3, wall=30min)
/implement <issue> --retry-cap <N>       override retry cap
/implement <issue> --wall-clock <mins>   override wall-clock budget
/implement <issue> --resume              continue an in-flight execution (rare)
```

## Preconditions

| Check | Refusal |
|-------|---------|
| `.claude/release-state.json` exists | "Run `/release-init <version>` first." |
| Issue in active project | "Issue #N is not in project <project-number>. Add it first." |
| `Phase == Ready` | "Issue is at <current>, not Ready. Use `/scope` + `/approve` first." |
| `AgentReady == Yes` | "Approval gate not met. Run `/approve <issue>` first." |
| §Sub-issues section absent or empty | "Issue is an umbrella with sub-issues. Delegate the children, not the parent." |
| Issue body has `## Implementation plan` | "Missing implementation plan — issue isn't fully scoped. Re-run `/scope`." |
| `gh auth status` has `repo`+`project` scopes | "Run `gh auth refresh -s project` (repo scope is standard)." |

## Worktree setup

```bash
# branch name derived from issue: <type>/issue-<N>-<slug>
git worktree add /tmp/aether-impl-<N> -b <type>/issue-<N>-<slug> main
cd /tmp/aether-impl-<N>
```

Worktree path is `/tmp/aether-impl-<N>` so concurrent `/implement` runs on different issues don't collide. Branch is cut from `main` (not the current branch) per the user's memory rule.

Type comes from the project item's `Type` field. Slug is the issue title sanitized: lowercased, alnum + dashes, max 30 chars.

## Execute phase

1. Set project item's `Phase = Executing`. Post audit comment: `[implement] Executing — branched <type>/issue-<N>-<slug> off main, worktree /tmp/aether-impl-<N>`.

2. Implement per the issue body's `## Implementation plan` section. The agent follows the plan literally: same files, same sequence, same test coverage. Deviations are bounces, not freelancing.

3. Run local pre-flight before pushing:
   - `cargo fmt`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo nextest run --workspace`
   - `cargo doc --workspace --no-deps`
   - wasm32 cross-build for component crates (`cargo metadata` → packages with `crate-type = cdylib` and `aether-component` dep)
   - `scripts/preflight.sh` if present (writes the stamp file expected by the pre-push hook)

4. Push the branch and open the PR:
   ```bash
   git push -u origin <branch>
   gh pr create --title "<conventional-commit title>" --body "<see PR body template below>"
   ```

5. Audit comment on issue: `[implement] PR opened: #<pr-number>`.

## Refine loop (the spin-until-green part)

After PR open, enter the loop. On each iteration:

1. Wait for CI to complete (`gh pr checks <pr> --watch`).

2. **CI green** → goto "Done condition" below.

3. **CI failed** → pull logs (`gh run view <run-id> --log-failed`), classify, act:

   ```
   Classification → Action
   ─────────────────────────────────────────────────────────────────
   Format / clippy / doc           → always real, mechanical fix
   Build error                     → always real, mechanical fix
   Same test fails twice in a row  → real failure, fix the cause
   Different test each attempt     → likely flake, rerun without push
   Scenario runner regression      → real, fix or bounce-to-Design
   Pre-existing test breaks        → likely scope expansion needed
                                     bounce-to-Plan with the test name
   Build env failure (qodana down,
   gh api rate limit, network)     → Stalled, abort loop, set
                                     Phase=Stalled, exit
   ```

4. If real failure, fix in the worktree, push to the same branch, increment attempt counter, goto step 1.

5. Set project item's `Phase` to `Refine` during fix-and-wait, back to `Executing` when pushing the fix. (Flicker is intentional — gives the board honest visibility.)

6. Audit comment per attempt: `[implement] CI failed (attempt <N>/<retry-cap>): <one-line summary>` and `[implement] Fix pushed for attempt <N>`.

7. **Retry cap hit** → self-bounce. `Phase=Bounced`, `BounceTo=Plan`, comment with the full attempt history.

8. **Wall-clock hit** → self-bounce. Same as retry cap with the elapsed time noted.

9. **Design-level discovery** at any attempt → self-bounce. `Phase=Bounced`, `BounceTo=Design`, comment with the specific finding. Examples:
   - "Approach X doesn't work because Y; needs alternative."
   - "Test Z passes only if we also change A, which is outside §Implementation plan."

## Flake detection (v1, simple)

Per-test counter. If test `foo::bar` fails on attempt 1, store it. If it fails again on attempt 2, real failure — fix the underlying cause. If different tests fail each attempt with no common cause, treat as flake — rerun CI (no push) up to 2 times before counting against retry budget.

Format/clippy/build are never flakes — always real, always immediate fix.

## Done condition

CI green:

1. Audit comment: `[implement] CI green on attempt <N>. PR #<pr-number> ready for merge.`
2. Project item: leave `Phase = Refine` and `AgentReady = Yes`. The issue is now in the "PR open + green" state; the merge is a separate human or orchestrator decision.
3. Do not merge. Do not close. Do not auto-set Phase=Done.
4. Print to user:

   ```
   ✓ #<N> implemented and CI-green.
   PR: <pr-url>
   Branch: <type>/issue-<N>-<slug>
   Worktree: /tmp/aether-impl-<N>  (clean up after merge with `git worktree remove`)
   Next: review and merge the PR; Phase will go to Done at merge.
   ```

Phase moves to `Done` either:
- When the user merges and a post-merge hook (or `/release-promote`) detects it, **or**
- When the Phase C orchestrator (future) merges under bounded auth.

For v1, that final transition is manual: the user merges via `gh pr merge` or the UI, then optionally runs `/release-promote <issue>` to mark Phase=Done. (Or it could just be inferred by Phase D tooling that reconciles state.)

## Self-bounce mechanics

Uses the same machinery as `/bounce` — see that skill's "Self-bounce by other skills" section. Audit comment prefix is `[implement]`:

```
[implement] Self-bounce after attempt <N>: Executing → Bounced (BounceTo=Plan).
   Reason: <retry cap hit | wall clock hit | scope expansion needed | ...>
   Attempts history:
     1. <failure summary>
     2. <failure summary>
     3. <failure summary>
```

The worktree stays on disk until the user cleans up — useful for inspecting the failed state. Worktree cleanup is *not* part of self-bounce.

## PR body template

```markdown
Closes iamacoffeepot/aether#<issue>.

## Summary

<extracted from issue body — the §Problem statement + chosen approach from §Design notes, condensed>

## Test plan

<extracted from §Implementation plan's test-coverage notes>

## Generated by

`/implement` — agent execution of [scoped issue #<issue>](<issue-url>).
```

The cross-repo close form (`Closes iamacoffeepot/aether#N`) is required because the bare `#N` form gets stripped by the user's PR-body hook. See `feedback_close_keyword_hook_strips_hash.md`.

## Auth budget (v1, will grow in Phase C)

| Budget | Default | Override |
|--------|---------|----------|
| Retry cap | 3 attempts after a real failure | `--retry-cap <N>` |
| Wall clock | 30 minutes total | `--wall-clock <mins>` |
| Token cost | not enforced in v1 | future `--token-cap <N>` |

Both caps trigger self-bounce to Plan with the budget breach noted. The `AuthBudget` field on the project item is the persistent record; for v1 it's a free-text note ("retry=3, wall=30m"). Phase C orchestrator will read this field to apply per-issue budgets.

## Failure modes

- **`release-state.json` stale**: rebuild via `/release-init <version> --reuse <num>`.
- **PR creation fails** (e.g. duplicate branch from prior aborted run): clean up the stale branch (`git branch -D`), retry. If repeated failure, self-bounce to Plan.
- **Pre-flight CI failure on first push** (formatting, build): fix in-worktree and push. Doesn't count against retry budget — local-equivalent failures are pre-CI.
- **Worktree creation fails** (path already exists from prior aborted run): `git worktree remove` the stale one, retry. If `git worktree list` is stuck, instruct the user to clean it up manually.
- **Phase regression while running** (someone hand-bounces the issue mid-execution): detect on next field-update, abort the loop, leave the branch and PR as-is, post a comment noting the abort.
- **PR gets reviewer comments mid-loop**: ignore in v1. `/implement` only listens to CI signal. Reviewer feedback is a separate human concern — they can `/bounce` or comment on the PR directly.

## What `/implement` does NOT do

- Merge the PR (manual or Phase C orchestrator).
- Edit the issue body (only `/scope` does).
- Re-scope the issue when CI surfaces problems — bounce instead.
- Address reviewer feedback on the PR. Reviewers comment; `/bounce` if the feedback requires re-scoping; manual handling otherwise.
- Notify anyone. Audit comments are the surface.
- Run on issues that aren't in the active project. Use `/delegate` for ad-hoc agent-labeled work outside the Project flow.
- Clean up worktrees after success or bounce. Leaves them for inspection; `git worktree remove /tmp/aether-impl-<N>` is the user's call.

## Comparison: `/delegate` vs `/implement`

| Concern | `/delegate` | `/implement` |
|---------|-------------|--------------|
| Required state | `agent` label | `Phase=Ready`, `AgentReady=Yes` |
| Project board awareness | none | full |
| CI loop | none (one-shot) | spins until green or bounce |
| Self-bounce | no | yes (to Plan or Design) |
| Auth budget | none | retry-cap + wall-clock |
| Auto-merge | no | no (parity) |
| Worktree | yes | yes |
| Use case | quick fixes, hot-takes | release-flow execution |
