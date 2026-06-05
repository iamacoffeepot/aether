---
name: implement
description: The single path from issue to open PR. Default mode requires Phase=Ready + AgentReady=Yes (post /approve); --quick skips the board gate for ad-hoc body-carries-the-fix issues. Cuts a worktree branch, implements the plan, opens a PR, loops CI until green, then holds for review (never auto-merges). Replaces the retired /delegate skill.
---

# /implement — the implementation skill

The single path from an issue to an open PR. Pairs with `/scope` and `/approve`: where those produce a vetted plan, `/implement` carries it out in a worktree, loops the Execute⇄Refine cycle internally until CI is green, then holds for merge.

Two entry shapes, one skill:

- **Scoped** — `/implement <issue>` — the issue passed `/scope` + `/approve` (`Phase=Ready`, `AgentReady=Yes`). The default release-flow path.
- **Quick** — `/implement <issue> --quick` — an ad-hoc fix whose issue body already carries a complete, mechanical fix. Skips the board approval gate and goes straight to Executing. This **replaces the retired `/delegate` skill** — same niche (small, mechanical, the body carries the fix), but executed in the main session rather than a dispatched isolated Agent (worktree-isolated Agents proved flaky — see `feedback_delegate_implementation_stop_after_commit`).

Always runs in the **main session**, never a dispatched Agent. It opens the PR **as a draft**, drives CI green, and holds it in draft for your review. This repo has native GitHub auto-merge on, so a *non-draft* PR that reaches green merges itself — draft is the review gate (see `feedback_green_pr_automerges_before_review`). Landing is the release *process*'s call: an approved release un-drafts the PR so native auto-merge takes it. This skill never issues a merge command and never un-drafts on its own.

## Invocation

```
/implement <issue>                       scoped run (defaults: retry-cap=3, wall=30min)
/implement <issue> --quick               ad-hoc fix: skip the Ready/AgentReady gate (body must carry a complete fix)
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

**`--quick` mode relaxes the gate.** With `--quick`, the `Phase == Ready` and `AgentReady == Yes` checks are skipped, and the issue need not be on the project board at all. In exchange, the issue body MUST carry a complete, mechanical fix — either a `## Implementation plan` section or an unambiguous proposed-fix description. Before proceeding, sanity-check the body:

- **Body ambiguous or missing the fix** → refuse: *"`--quick` needs a complete fix in the body. Run `/scope <issue>` to design it."* Don't guess.
- **Fix looks design-bearing** (new public API, wire-format change, ADR-worthy choice) → refuse: *"This needs design, not a quick fix. Run `/scope <issue>`."* `--quick` is for mechanical work only (the old `/delegate` bar).
- **Issue not on the active project board** → run **label-only**: set the `phase:*` labels normally but skip every `Phase` / `AgentReady` board-field write (there's no project item to update). All other behavior is identical.

## Worktree setup

```bash
# branch name derived from issue: <type>/issue-<N>-<slug>
git worktree add .claude/worktrees/issue-<N> -b <type>/issue-<N>-<slug> main
cd .claude/worktrees/issue-<N>
```

Worktree path is `.claude/worktrees/issue-<N>` (gitignored per CLAUDE.md §Workflow) so concurrent `/implement` runs on different issues don't collide. Branch is cut from `main` (not the current branch) per the user's memory rule.

Type comes from the project item's `Type` field. Slug is the issue title sanitized: lowercased, alnum + dashes, max 30 chars.

## Execute phase

1. Set project item's `Phase = Executing` and reconcile the issue label to `phase:executing` (see [Phase label reconcile](#phase-label-reconcile)). In `--quick` label-only mode (issue not on the board), set the label and skip the board field. Post audit comment: `[implement] Executing — branched <type>/issue-<N>-<slug> off main, worktree .claude/worktrees/issue-<N>`.

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
   gh pr create --draft --title "<conventional-commit title>" --body "<see PR body template below>"
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

5. Set project item's `Phase` to `Refine` during fix-and-wait, back to `Executing` when pushing the fix. (Flicker is intentional — gives the board honest visibility.) Reconcile the `phase:*` label on each flip (see [Phase label reconcile](#phase-label-reconcile)).

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

1. Audit comment: `[implement] CI green on attempt <N>. Draft PR #<pr-number> ready for your review.`
2. Project item: leave `Phase = Refine` and `AgentReady = Yes` (the issue label stays `phase:refine`). The resting state is "draft PR open + green".
3. Leave the PR as a **draft**. Do not un-draft, do not merge, do not close, do not auto-set Phase=Done. Un-drafting is the user's (or the approved release process's) action — once a PR is un-drafted, native auto-merge lands it on green ([[feedback_green_pr_automerges_before_review]]).
4. Print to user:

   ```
   ✓ #<N> implemented and CI-green.
   Draft PR: <pr-url>
   Branch: <type>/issue-<N>-<slug>
   Worktree: .claude/worktrees/issue-<N>  (clean up after merge with `git worktree remove`)
   Next: review the draft; un-draft (or tell me) to let native auto-merge land it on green. Phase → Done at merge.
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

## Phase label reconcile

The board `Phase` field is only visible on the project board — not on the issue itself or in `gh issue list`. This skill mirrors every Phase write as a `phase:*` label on the issue so the lifecycle is legible at a glance, and the label never disagrees with the board. **In the same step you set the `Phase` field, reconcile the label:**

```bash
gh issue edit <n> --remove-label "phase:define,phase:design,phase:plan,phase:ready,phase:executing,phase:refine,phase:bounced,phase:stalled"
gh issue edit <n> --add-label "phase:<new>"
```

The remove and add are **two separate invocations** on purpose: a single `gh issue edit` with the target listed in both `--remove-label` and `--add-label` strips it and never re-adds it (the remove wins), so the issue ends up with no phase label on every real transition. Splitting the calls makes the add unconditional. `--remove-label` ignores labels the issue doesn't carry, so the first line is safe on any transition. This skill writes `Phase` in four places, each of which must reconcile the label: `Executing` (Execute step 1 + Refine-loop fix-push), `Refine` (Refine-loop fix-and-wait + done condition), `Bounced` (self-bounce on retry-cap / wall-clock / design discovery), and `Stalled` (build-env failure). `Done` carries no label — the merge that closes the issue retires the lifecycle.

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
- Merge — code PRs always hold for your review; auto-merge is the release process's call, not this skill's.
- Run scoped (without `--quick`) on issues that aren't in the active project. For an ad-hoc fix outside the board, use `--quick` (label-only mode).
- Clean up worktrees after success or bounce. Leaves them for inspection; `git worktree remove .claude/worktrees/issue-<N>` is the user's call.
