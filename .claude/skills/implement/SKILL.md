---
name: implement
description: The single path from issue to open PR. Default mode requires the issue at phase:ready (post /approve); --quick skips the phase:ready gate for ad-hoc body-carries-the-fix issues. Cuts a worktree branch, implements the plan, opens a PR, loops CI until green, then holds for review (never auto-merges). Replaces the retired /delegate skill.
---

# /implement — the implementation skill

The single path from an issue to an open PR. Pairs with `/scope` and `/approve`: where those produce a vetted plan, `/implement` carries it out in a worktree, spins the CI loop until green, then holds the draft for merge. The post-Ready resting states — `building` → `qa` → `findings` → `held` — are computed by the reconciler workflow from the PR as it goes; `/implement` writes none of them.

Two entry shapes, one skill:

- **Scoped** — `/implement <issue>` — the issue passed `/scope` + `/approve` (`phase:ready`). The default release-flow path.
- **Quick** — `/implement <issue> --quick` — an ad-hoc fix whose issue body already carries a complete, mechanical fix. Skips the `phase:ready` approval gate and goes straight to the work. This **replaces the retired `/delegate` skill** — same niche (small, mechanical, the body carries the fix), and runs in the main session (a `--quick` fix is too small to be worth a worktree hand-off; the hybrid background-agent split below is the sanctioned way to delegate the scoped path — see `feedback_delegate_implementation_stop_after_commit`).

Two ways to run it:

- **In-session (default).** The whole skill runs in the main session — implement, push, drive CI green, hold the draft. Use this for a single issue or when you want tight control over each step.
- **Hybrid background-agent.** To parallelize across independent issues, the orchestrator may dispatch one background Agent per issue that does *only* the bounded, parallelizable part: cut the worktree off `main`, implement the plan, run `cargo fmt`, commit (step 3), and assert a clean working tree (`git status --porcelain` empty) — then **STOP** after committing. The commit lands within the agent's turn, so an agent that exhausts its turn still leaves committed, reviewable work. The clean-working-tree assertion guarantees the committed HEAD is exactly what the parent adopts: because `git stash` is banned for concurrent agents (it is repo-global and crosses worktrees — `feedback_concurrent_agents_never_git_stash`), any post-commit edits must be amended into the commit (or discarded), never stashed away. GitHub is the build engine — no heavy checks run locally on either side; `cargo fmt` is the only local step, and CI runs the full check set on every push as the sole gate. The main session ("parent") then takes each finished worktree and runs the serial, less-reliable part itself: the push, the draft-PR open, and the CI-green Refine loop — reviewing the agent's diff as it takes over. Never hand the push, PR creation, CI loop, or phase-label writes to the dispatched Agent: handing off the *whole* skill (the retired `/delegate`) proved flaky, so the split keeps the unreliable parts in-session (see `feedback_delegate_implementation_stop_after_commit`). **Neither the agent nor the parent writes a phase label during the implement window.** The post-Ready ladder is the reconciler's: it sets `phase:building` when the PR opens (the `pull_request` open event) and recomputes from CI state on every push. The dispatched agent never touches the phase label — it never did — and the parent no longer flips one at dispatch, so an issue reads `phase:ready` until its PR exists, at which point the reconciler moves it to `building`. The double-dispatch that a phase flip once guarded against is now caught by the stale-worktree and open-PR probe below: a re-sweep of an issue whose agent is still mid-flight finds the worktree ahead or PR-attached and surfaces it rather than re-dispatching.

  **Batched dispatch.** Before spawning agents, the orchestrator reads each candidate issue's `size:*` and `model:*` labels over REST (`gh api repos/iamacoffeepot/aether/issues/<n> --jq '.labels[].name'` per issue, or one `gh api 'repos/iamacoffeepot/aether/issues?labels=size:m&state=open' --jq '.[].number'` sweep — not `gh issue view` / `gh issue list`, which are GraphQL-backed) — no GraphQL query needed, since `/scope` stamps the size and model routing onto labels at Plan time. It then packs the approved issues into per-agent queues by an **estimated context budget** rather than a fixed count: a queue accumulates issues until the next one would push it past the per-model context budget, then a new queue opens. The budget is selected from the queue's `model:*` key (which is already the packing key — queues never mix models): opus packs against the full **~150k** budget; sonnet packs against a lower **~100k** budget, reserving headroom for the estimate-to-actual drift sonnet's tighter context window cannot absorb.

  The `size:*` label is the prior context-cost estimate — heuristic anchors **S ≈ 25k, M ≈ 60k, L ≈ 120k** accumulated agent context (exploration + diff churn) — and reading each candidate's body and `## Implementation plan` refines it: step count, the count of files and crates the plan touches, and how much exploration the change implies all move the estimate off its label anchor. The anchors are model-independent — they price the work, not the runner — so the same plan reads the same cost estimate under either model; only the per-model ceiling from above differs, meaning a sonnet queue packs fewer issues solely because its ceiling is lower. Pack greedily against the refined estimates: smalls pack densely (several trivial S can share one agent where the old count rule capped at three), mediums co-queue when two fit under the cap, and an L stays solo because its prior alone approaches the threshold.

  Co-queue only under **crate affinity** — issues that share a `crate:*` label or carry an explicit relates-to link — so the shared exploration context an agent builds for the first issue pays off on the next. Issues with no affinity are dispatched one agent each, in parallel; batching unrelated work just piles unreusable context into one queue. The exception is trivial mechanical no-crate work (a doc tweak, a label fix, a one-line config change): co-queue it regardless of crate, since its context residue is noise and the per-agent dispatch overhead dominates the cost. Order each queue **broadest-exploration-first** — the issue needing the widest read goes at the head, so the shared context is paid for once and the cheaper issues behind it reuse it.

  Each queued issue is still a full single-issue `/implement` run — its own worktree, its own draft PR; packing only decides how many of those runs one background agent works through before it spins down, so a pile of small mechanical work doesn't spin up one full agent each (fewer concurrent agents also staggers the shared per-user GraphQL budget). The `model:*` label routes the agent's model and is **required**: `/scope` stamps it at Plan and `/approve` gates on it, so a scoped candidate with no `model:*` label is dispatch-ineligible — drop it with reason "no model label, re-run /scope Plan or stamp by hand", never fall back to the dispatcher's own model. Issues sharing one queue must share one model (model is part of the packing key). See `/scope` §Plan size-estimation and model-routing notes.

Either mode opens the PR **as a draft**, drives CI green, and holds it in draft for your review. This repo has native GitHub auto-merge on, so a *non-draft* PR that reaches green merges itself — draft is the review gate (see `feedback_green_pr_automerges_before_review`). Landing is the release *process*'s call: an approved release un-drafts the PR so native auto-merge takes it. This skill never issues a merge command and never un-drafts on its own.

## Sweep dispatch

`/implement --sweep` is the batched hybrid background-agent entry point: it discovers the eligible set instead of taking one issue, packs it into per-agent queues, and waits for your confirmation before any agent spawns. It exists so the orchestrator stops assembling each dispatch set by hand.

1. **Enumerate over REST, in one call.** `phase:ready` is set only by `/approve` — so the label alone is the eligibility signal, queried over REST and off the contended GraphQL pool:

   ```bash
   gh api 'repos/iamacoffeepot/aether/issues?labels=phase:ready&state=open' --jq '.[].number'
   ```

   This is the REST issues endpoint (per `/scope` §REST-vs-GraphQL routing), not `gh issue list`, which is GraphQL-backed and drains the contended pool.

2. **Gate-check each candidate.** Run the same [per-issue preconditions](#preconditions) the single-issue path runs — `phase:ready` present, no `## Sub-issues` umbrella, `## Implementation plan` present, exactly one `model:*` label. Drop any issue that fails and record the reason; the sweep does not silently skip — every dropped issue is listed in the plan with its drop reason.

3. **Pack and order.** Apply the **Batched dispatch** rules above (under the hybrid background-agent mode): budget-based packing against the `size:*`-label priors refined by each body read, crate-affinity co-queueing with the trivial-mechanical exception, broadest-exploration-first ordering within each queue. Concurrency equals the number of packed queues, bounded by the per-model context-budget packing threshold (§Batched dispatch), not a flat agent count — the binding axis is per-agent context, not the REST pool.

   **Stale-worktree probe.** A re-swept issue from a prior bounced or aborted attempt can leave a stale `.claude/worktrees/issue-<N>` worktree and branch behind, so probe each packed candidate before dispatch: does the worktree exist, how many files are uncommitted in it, is its branch ahead of `origin/main`, and is there an open PR for the head branch (the REST `pulls?head=` form, never `gh pr list`):

   ```bash
   main_root="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"; wt="$main_root/.claude/worktrees/issue-<N>"; br=<type>/issue-<N>-<slug>
   dirty=$(git -C "$wt" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
   ahead=$(git -C "$wt" rev-list --count origin/main..HEAD 2>/dev/null)
   pr=$(gh api "repos/iamacoffeepot/aether/pulls?head=iamacoffeepot:$br&state=open" --jq '.[].number')
   ```

   Classify each: **safe to auto-clear** when the worktree is clean (`dirty == 0`), its branch is not ahead of `origin/main` (`ahead == 0`), and there is no open PR — clear it at dispatch with `git worktree remove "$wt"` plus `git branch -D "$br"`. **Flag** when any of uncommitted files, unpushed commits, or an open PR is present — clearing would discard bounce context or unpushed work, so surface it as a plan line item rather than clearing, and let the one confirmation prompt the sweep already prints cover the destructive decision.

   **File-overlap probe.** Two candidates that both edit the same file are a guaranteed land-time merge conflict — `/land`'s merge-tree oracle will surface it, but only after implementation has already happened. After packing, extract each candidate's declared edit paths from its scope body — the file paths cited in `## Implementation plan` (each edit-site names its file path per `/scope`'s convention) and the surfaces listed in `## Design notes` §Affected surfaces — and compute the pairwise path intersection across the full batch (not just within a queue: each issue lands as its own PR regardless of which agent runs it). An exact path shared by two candidates is a confident flag; a directory- or pattern-shaped citation (e.g. "every file in `crates/aether-data/`") yields a softer "possible overlap" note. The check is advisory only — never a blocker, never a re-pack input: overlap is a frequent, legitimate state (a refactor arc deliberately touches the same file across issues), and the user's confirmation gate already covers the destructive decision. It complements `/land`'s authoritative merge-tree oracle; at dispatch time no branch exists yet, so a content-level merge-tree check is impossible — the body-path heuristic is the only signal available that early.

4. **Print the dispatch plan and wait for confirmation.** Packing is heuristic, so a mis-packed multi-issue agent run is expensive to unwind — one confirmation prompt per sweep is cheap insurance. Print the queues, their issues in order, the routed model per queue, the stale-worktree classification per affected candidate, and the dropped-with-reason list, then stop and wait:

   ```
   Sweep: 7 phase:ready issues, 3 dropped, 4 dispatched across 2 agents.

   Agent 1 (model: opus)     ~110k  [crate:aether-data]
     #1612  refactor kind-id newtype helpers        (broadest — read first)
     #1613  thread the helper through the decoder
   Agent 2 (model: sonnet)   ~70k   [trivial mechanical]
     #1631  fix the doc link in fs.md
     #1633  drop the stale config knob

   Stale worktrees:
     #1612  clean, branch at origin/main, no PR → auto-clear at dispatch
     #1631  2 uncommitted files → FLAG: clearing loses bounce context, confirm

   File overlap:
     .claude/skills/implement/SKILL.md  →  #1631 × #1633  (exact path match — land-time conflict)
     crates/aether-data/**              →  #1612 × #1613  (pattern — possible overlap)

   Dropped:
     #1620  Phase=Design, not Ready
     #1622  no ## Implementation plan
     #1607  umbrella (has ## Sub-issues)

   Confirm dispatch? (the agents spawn only on your go-ahead)
   ```

   Candidates with no stale worktree need no line. Omit the **Stale worktrees** block entirely when none of the dispatched candidates have one. Omit the **File overlap** block entirely when the pairwise path intersection across the batch is empty.

5. **On confirmation, dispatch.** Clear the stale worktrees first: the auto-clear set unconditionally, and any flagged set the user confirmed (`git worktree remove` plus `git branch -D` per candidate) so each agent's `git worktree add` starts clean.

   Then spawn one background agent per queue, each working its queue's issues in order as full single-issue `/implement` runs that stop after commit. No phase label is written at dispatch — the reconciler sets `phase:building` when each PR opens (see the hybrid background-agent paragraph). The parent then takes over each finished worktree per the hybrid split: push, open the draft PR, and drive the CI-green Refine loop. GitHub runs every check on each push, so nothing heavy builds locally; each worktree keeps its own private `target/` — no `CARGO_TARGET_DIR` override — per #2202.

The sweep never auto-confirms and never dispatches the serial tail (push / PR / CI loop / phase-label writes) to an agent — it only assembles and confirms the batch the hybrid mode then runs.

## Invocation

```
/implement <issue>                       scoped run (defaults: retry-cap=3, wall=30min)
/implement --sweep                       enumerate every phase:ready issue, pack per-agent queues, confirm, dispatch
/implement <issue> --quick               ad-hoc fix: skip the phase:ready gate (body must carry a complete fix)
/implement <issue> --retry-cap <N>       override retry cap
/implement <issue> --wall-clock <mins>   override wall-clock budget
/implement <issue> --resume              continue an in-flight execution (rare)
```

`--sweep` takes no issue argument — it discovers them. It is the batched hybrid background-agent entry point: one REST enumeration of the eligible set, budget-based packing into per-agent queues, a confirmation gate, then dispatch. See [Sweep dispatch](#sweep-dispatch).

## Preconditions

| Check | Refusal |
|-------|---------|
| `phase:ready` label present | "Issue is not Ready (no `phase:ready` label). Use `/scope` + `/approve` first." |
| §Sub-issues section absent or empty | "Issue is an umbrella with sub-issues. Delegate the children, not the parent." (The malformed-umbrella case — a non-empty `## Sub-issues` alongside a substantial own plan — is refused upstream at `/approve`'s Umbrella integrity gate, so any issue that reaches `/implement` with a non-empty `## Sub-issues` is a pure umbrella and correct to drop.) |
| Exactly one `model:*` label | "Missing model:* label (or more than one). Re-run `/scope`'s Plan step or stamp the label by hand." |
| Issue body has `## Implementation plan` | "Missing implementation plan — issue isn't fully scoped. Re-run `/scope`." |
| `gh auth status` has `repo` scope | "Run `gh auth refresh` (repo scope is standard)." |

**`--quick` mode relaxes the gate.** With `--quick`, the `phase:ready` and `model:*`-label checks are skipped (a `--quick` fix runs in the main session — no agent is dispatched, so there is nothing to route). In exchange, the issue body MUST carry a complete, mechanical fix — either a `## Implementation plan` section or an unambiguous proposed-fix description. Before proceeding, sanity-check the body:

- **Body ambiguous or missing the fix** → refuse: *"`--quick` needs a complete fix in the body. Run `/scope <issue>` to design it."* Don't guess.
- **Fix looks design-bearing** (new public API, wire-format change, ADR-worthy choice) → refuse: *"This needs design, not a quick fix. Run `/scope <issue>`."* `--quick` is for mechanical work only (the old `/delegate` bar).

**Bench the issue before working it.** A `--quick` run works in the main session while the cloud fleet's wavefront can still see the issue on the board, so the first act after the gate check is to bench it:

```bash
gh api -X POST repos/iamacoffeepot/aether/issues/<n>/labels -f 'labels[]=agent:dont-touch'
```

`agent:dont-touch` is the fleet's per-issue kill switch — the tick's scan filters it and agent-work's point-of-spend guard refuses it — so no wavefront tick dispatches the same issue mid-run (the tick counted a quick issue as dispatchable before this rule; #3433). When the quick issue is filed in-session rather than pre-existing, stamping the label in the creation call is equivalent. No removal step: only the owner removes the label, and the merge that closes the issue retires it with the lifecycle.

## Worktree setup

```bash
# branch name derived from issue: <type>/issue-<N>-<slug>
main_root="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
git worktree add "$main_root/.claude/worktrees/issue-<N>" -b <type>/issue-<N>-<slug> origin/main
cd "$main_root/.claude/worktrees/issue-<N>"
```

The worktree is pinned to the **main working tree** via `main_root`, derived cwd-invariantly from `--git-common-dir`: `dirname` of the absolute `--git-common-dir` path is always the main repo root regardless of which worktree is current or what the agent's cwd is. `--show-toplevel` is the current worktree's root — from inside a nested session worktree it returns the session worktree, not the main repo, making any path derived from it cwd-dependent and subject to the drift this anchor avoids. All commands that create or act on the per-issue worktree use `"$main_root/.claude/worktrees/issue-<N>"`, so create site and act-on site resolve to the same directory. This satisfies the CLAUDE.md §Workflow contract that worktrees live under the project root's `.claude/worktrees/`.

Worktree path is `.claude/worktrees/issue-<N>` (gitignored per CLAUDE.md §Workflow) so concurrent `/implement` runs on different issues don't collide. Branch is cut from `main` (not the current branch) per the user's memory rule.

Before the `git worktree add`, run the same [stale-worktree probe](#sweep-dispatch) §Sweep dispatch uses, for this one issue: if `.claude/worktrees/issue-<N>` already exists from a prior aborted or bounced attempt, check its uncommitted-file count, whether its branch is ahead of `origin/main`, and whether an open PR is attached (the REST `pulls?head=` form). Auto-clear when safe — clean worktree, branch not ahead, no open PR — with `git worktree remove` plus `git branch -D`, then proceed with the add. Surface and stop when the worktree is dirty, ahead, or PR-attached: clearing would discard uncommitted bounce context or unpushed work, so report the state and let the user decide rather than forcing the add.

Ground every read against the current ref per `/scope`'s canonical [Grounding against `origin/main`](../scope/SKILL.md#grounding-against-originmain) section — fast-forward to `origin/main` before reading, verify `HEAD == origin/main`, and treat a surprise call site as a staleness smell to diff before escalating. The worktree this skill cuts is branched from `main`, and a per-agent tree can be cut before a sibling PR lands, so without this an implement run grounds its work against code that has already changed on main.

Type comes from the issue's `type:*` label. Slug is the issue title sanitized: lowercased, alnum + dashes, max 30 chars.

## Execute phase

1. No phase-label write here. The post-Ready ladder is the reconciler's: it sets `phase:building` when the PR opens (step 5) and recomputes from CI on every push, so the Execute phase opens straight into the work with the issue still at `phase:ready`. The dispatched agent begins here in both modes; nothing flips a label at dispatch (see the hybrid background-agent paragraph above).

2. Implement per the issue body's `## Implementation plan` section. The agent follows the plan literally: same files, same sequence, same test coverage. Deviations are bounces, not freelancing.

3. Commit the work. The committed HEAD is the durable handoff artifact the parent's takeover adopts. In the in-session path this step runs inline and the agent continues to step 4; in hybrid background-agent mode the agent commits here and proceeds to step 4.

4. Format before pushing. GitHub is the build engine and runs the full check set — clippy, docs, marker build, tests, dup-check, unused-deps — on every push, so it is the sole gate and nothing heavy builds locally. In hybrid background-agent mode the agent runs this step, then **STOPs** after it; in the in-session path it runs here before step 5.
   - Assert a clean working tree (`git status --porcelain` empty) — the committed HEAD is what gets pushed; amend any post-commit edits into the commit (or discard them) before proceeding; `git stash` is banned for concurrent agents (`feedback_concurrent_agents_never_git_stash`)
   - `cargo fmt` — the one local check. A formatting slip is the cheapest CI red to avoid; every other failure surfaces in the Refine loop and is fixed there.

5. Push the branch, then open the PR over REST (`gh pr create` is GraphQL-backed; `POST …/pulls` is REST and takes `draft: true` directly). Write the PR body to a file first so backticks / `$` in the template aren't shell-expanded, and pass it with `-F body=@<file>`:
   ```bash
   git push -u origin <branch>
   gh api -X POST repos/iamacoffeepot/aether/pulls \
     -F draft=true \
     -f title="<conventional-commit title>" \
     -f head="<branch>" -f base=main \
     -F body=@/tmp/pr-body-<N>.md \
     --jq '.number'
   ```

   No "PR opened" comment — the PR body's closing reference creates a cross-reference event in the issue's timeline. Capture the returned `number` for the Refine loop.

## Refine loop (the spin-until-green part)

After PR open, enter the loop. On each iteration:

1. Wait for CI to complete. `gh pr checks --watch` polls GraphQL on every tick, draining the contended GraphQL pool, so poll the REST check-runs endpoint instead — and run from the script file rather than inline, since the harness hook that scans command text for `$(…)` / `$…$` spans trips on an inline poller (see `feedback_monitor_ci_via_rest_not_watch`):

   ```bash
   scripts/wave-status.sh --wait <pr>
   ```

   `wave-status.sh --wait <pr>` loops (polling every 20s) until `CI pass` — the required merge aggregator — is present and completed with zero pending check-runs, then exits 0 on `success` or 1 on failure/neutral. A subset-registered matrix (only `Detect changes` up, say) can't trip a false green. To respond to reds as they surface rather than after the whole run settles, it **fast-fails**: the moment a deterministic check (Format / Clippy / Docs / Marker-only host build) concludes failure it exits 1 immediately, without waiting for the slow test jobs — those checks are never flaky and never auto-retried to green, so their red already dooms `CI pass`. Exit 0 → goto step 2; exit 1 → the script has already printed the failed check names — go to step 3.

2. **CI green** → goto "Done condition" below.

3. **CI failed** (a required check is red) → pull logs (`gh run view <run-id> --log-failed`), classify, act:

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
   Build env failure (gh api rate
   limit, network)                 → Stalled, abort loop, set
                                     Phase=Stalled, exit
   ```

4. If real failure, fix in the worktree, push to the same branch, increment attempt counter, goto step 1. The push supersedes any still-running jobs from the prior sha — GitHub cancels the superseded run and starts fresh, so an early fix costs nothing.

5. No phase-label writes in the loop. The reconciler computes `phase:building` / `phase:qa` from the PR's CI state on every push — a fix push demotes the PR to `building`, a green head advances it to `qa` — so the loop touches no label. No per-attempt comments either — the PR's own commit and check history is the attempt record; track the attempt counter in-session.

6. **Retry cap hit** → self-bounce. `phase:bounced`, `bounce-to:plan` label, comment with the full attempt history.

7. **Wall-clock hit** → self-bounce. Same as retry cap with the elapsed time noted.

8. **Design-level discovery** at any attempt → self-bounce. `phase:bounced`, `bounce-to:design` label, comment with the specific finding. Examples:
   - "Approach X doesn't work because Y; needs alternative."
   - "Test Z passes only if we also change A, which is outside §Implementation plan."

## Flake detection (v1, simple)

Per-test counter. If test `foo::bar` fails on attempt 1, store it. If it fails again on attempt 2, real failure — fix the underlying cause. If different tests fail each attempt with no common cause, treat as flake — rerun CI (no push) up to 2 times before counting against retry budget.

Format/clippy/build are never flakes — always real, always immediate fix.

## Done condition

CI green:

1. No phase-label write. The reconciler computes the resting state from the PR: `phase:held` when CI is green and nothing is open, or `phase:findings` when the requested review/dogfood rollup posts actionable findings. `/implement` writes nothing here — the draft-PR-open-and-green fact is what the reconciler reads.
2. **Request the review.** CI green is the explicit hand-off to review — the review never fires on its own, so a green PR that nobody requests a review for sits verdict-less forever:

   ```bash
   gh workflow run review.yml -f pr=<N>
   ```

   The dispatch activates the review against `origin/main...HEAD` — the reviewer's own tier table decides how much machinery the diff earns (issue #3404; the requester never pre-judges that) — critic submits one `APPROVE` / `REQUEST_CHANGES` verdict, and dogfood chains off the review's completion. On the headless implement box the dispatch rides the fleet App token (App-created events trigger downstream workflows).
3. **The verdict loop (issue #3405).** The builder is not done until its work is accepted — wait for the verdict and loop until APPROVE:

   ```bash
   scripts/wave-status.sh --wait-verdict <PR>
   ```

   Exit 0 (APPROVED) → step 4. Exit 1 (CHANGES_REQUESTED) → run one findings iteration: execute [`/findings`' Mandate](../findings/SKILL.md#mandate) — fix or justify each finding, reply on its thread with the fix commit, resolve the thread — re-enter the [Refine loop](#refine-loop-the-spin-until-green-part) until CI is green on the fixed head, post the plain `@iamacritic review` re-request (the reviewer resolves it to the cheap in-session delta confirm), then return to the wait.

   **The crash-safety invariant:** every iteration completes its externally-visible acts — the fix pushes, the thread resolutions, the re-request comment — *before* re-entering the wait. A box that dies waiting strands nothing: the re-request is already posted, the confirm verdict arrives without any builder session, the reconciler computes the resting phase, and `/land` proceeds. The wait only buys the next iteration a warm context.

   **Budgets:** at most **3 findings iterations** per run — a fourth CHANGES_REQUESTED self-bounces to Plan with the full attempt history (the same mechanics as the retry cap). Within **10 minutes of the runner leash** (headless: the 90-minute `agent-work` timeout), finish the current iteration's visible acts, post the loop state as a PR comment, and end instead of re-entering the wait — the pipeline converges without the box, and a re-dispatched run's re-entrancy guard resumes from the observed PR state.
4. Leave the PR as a **draft**. Do not un-draft, do not merge, do not close, do not delete the `phase:*` label (Done is a `/land`-time action). Un-drafting is the user's (or the approved release process's) action — once a PR is un-drafted, native auto-merge lands it on green ([[feedback_green_pr_automerges_before_review]]).
   The verdict the loop ended on is critic's native APPROVE (with `dogfood:unresolved` still the dogfood runner's own contract). The reconciler computes the resting phase from the same facts the loop made true. The standalone `/findings <pr>` remains the manual path for a findings-phase PR whose builder box is gone (a leash death) or for review-only passes. `/land` refuses to land while critic's verdict stands at `REQUEST_CHANGES`, `dogfood:unresolved` is present, or the issue sits at `phase:findings`.
5. Print to user:

   ```
   ✓ #<N> implemented and CI-green.
   Draft PR: <pr-url>
   Branch: <type>/issue-<N>-<slug>
   Worktree: .claude/worktrees/issue-<N>
   Clean up after merge: main_root="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"; git worktree remove "$main_root/.claude/worktrees/issue-<N>"
   Next: review the draft; un-draft (or tell me) to let native auto-merge land it on green. Phase → Done at merge.
   ```

Phase moves to `Done` either:
- When the user merges and a post-merge hook (or `/release-promote`) detects it, **or**
- When the Phase C orchestrator (future) merges under bounded auth.

For v1, that final transition is manual: the user merges via the UI or `gh api -X PUT repos/iamacoffeepot/aether/pulls/<pr>/merge -f merge_method=squash` (REST; `gh pr merge` is the GraphQL-backed convenience form), then optionally runs `/release-promote <issue>` to mark it Done (delete the `phase:*` label). (Or it could just be inferred by Phase D tooling that reconciles state.)

## Self-bounce mechanics

Uses the same machinery as `/bounce` — see that skill's "Self-bounce by other skills" section. The bounce comment is prose markdown carrying the reason and the full attempt history (the one place that history lives):

```markdown
**Bounced to Plan** — retry cap hit after attempt <N>.

Attempts:

1. <failure summary>
2. <failure summary>
3. <failure summary>

<what the plan needs to address before a re-run>
```

The worktree stays on disk until the user cleans up — useful for inspecting the failed state. Worktree cleanup is *not* part of self-bounce.

## PR body template

```markdown
Closes #<issue>.

## Summary

<extracted from issue body — the §Problem statement + chosen approach from §Design notes, condensed>

## Test plan

<extracted from §Implementation plan's test-coverage notes>

## Generated by

`/implement` — agent execution of [scoped issue #<issue>](<issue-url>).
```

## Auth budget (v1, will grow in Phase C)

| Budget | Default | Override |
|--------|---------|----------|
| Retry cap | 3 attempts after a real failure | `--retry-cap <N>` |
| Wall clock | 30 minutes total | `--wall-clock <mins>` |
| Token cost | not enforced in v1 | future `--token-cap <N>` |

Both caps trigger self-bounce to Plan with the budget breach noted in the bounce comment. v1 does not persist the budget anywhere; a future Phase C orchestrator can reintroduce a per-issue budget store (a label, or a body field) when it needs one.

## Phase label reconcile

The `phase:*` label is the canonical phase state — the only phase store the pipeline keeps, legible on the issue itself and discoverable over the REST issues endpoint. The swap rides REST: `gh issue edit --add-label/--remove-label` is GraphQL-backed, while the `gh api …/labels` endpoints are REST, so the phase write stays off the contended pool.

```bash
# Atomic swap to an active phase. Runs under bash for array word-splitting.
bash <<'EOF'
n=<n>; new="phase:<new>"; repo=iamacoffeepot/aether
args=()
while IFS= read -r l; do args+=(-f "labels[]=$l"); done < <(
  gh api "repos/$repo/issues/$n/labels" --jq '.[].name | select(startswith("phase:") | not)')
args+=(-f "labels[]=$new")
gh api -X PUT "repos/$repo/issues/$n/labels" "${args[@]}"
EOF
```

The single `PUT …/labels` replaces the label set with the non-`phase:*` labels plus the one new `phase:*`, so the issue never carries two phase labels and never carries zero — a tighter guarantee than a remove-then-add pair, which has a window between its two calls. A failed PUT leaves the prior labels untouched and heals on the next run. This skill writes the phase label in exactly two places, both terminal: `phase:bounced` (self-bounce on retry-cap / wall-clock / design discovery) and `phase:stalled` (build-env failure). The post-Ready resting states — `phase:building` / `phase:qa` / `phase:findings` / `phase:held` — are the reconciler's, computed from the PR's CI and review state; `/implement` never writes them. `Done` carries no label — the merge that closes the issue retires the lifecycle (`/land` deletes the label).

## Failure modes

- **PR creation fails** (e.g. duplicate branch from prior aborted run): clean up the stale branch (`git branch -D`), retry. If repeated failure, self-bounce to Plan.
- **CI red on first push** (formatting, build, clippy): fix in-worktree and re-push. A first-push red doesn't count against the retry budget — with `cargo fmt` the only local check, shaking the initial build out under CI is expected, and the retry cap counts real failures once the PR is established.
- **Stale worktree from a prior aborted or bounced run** (`.claude/worktrees/issue-<N>` already exists): the [stale-worktree probe](#sweep-dispatch) catches this before `git worktree add` runs — auto-cleared when safe (clean, branch not ahead, no open PR), surfaced for a decision when dirty / ahead / PR-attached — both in §Sweep dispatch for the batch and inline in §Worktree setup for a single-issue run. If `git worktree list` is itself wedged so the remove can't proceed, instruct the user to clean it up manually.
- **Phase regression while running** (someone hand-bounces the issue mid-execution): detect on the next phase-label swap, abort the loop, leave the branch and PR as-is, post a comment noting the abort.
- **PR gets reviewer comments mid-CI-loop**: the Refine loop listens only to CI signal; critic's findings are consumed at their proper station — the verdict loop (Done condition step 3), after CI is green. A human reviewer's ad-hoc comments remain a human concern (`/bounce` or direct handling).

## What `/implement` does NOT do

- Merge the PR (manual or Phase C orchestrator).
- Edit the issue body (only `/scope` does).
- Re-scope the issue when CI surfaces problems — bounce instead.
- Address a *human* reviewer's ad-hoc feedback on the PR. Critic's findings are the verdict loop's input (Done condition step 3); a human's comments are `/bounce` material if they require re-scoping, manual handling otherwise.
- Notify anyone. The printed output and the `phase:*` labels are the surface; the only comment this skill posts is a self-bounce reason.
- Merge — code PRs always hold for your review; auto-merge is the release process's call, not this skill's.
- Run scoped (without `--quick`) on an issue that isn't at `phase:ready`. For an ad-hoc fix whose body already carries the change, use `--quick`.
- Clean up worktrees after success or bounce. Leaves them for inspection; `main_root="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"; git worktree remove "$main_root/.claude/worktrees/issue-<N>"` is the user's call (`/sweep worktrees` automates this at merge). Worktrees stay cheap on disk — `cargo fmt` is the only local command, so no `target/` build tree accumulates.
