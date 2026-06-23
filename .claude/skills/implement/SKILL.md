---
name: implement
description: The single path from issue to open PR. Default mode requires the issue at phase:ready (post /approve); --quick skips the phase:ready gate for ad-hoc body-carries-the-fix issues. Cuts a worktree branch, implements the plan, opens a PR, loops CI until green, then holds for review (never auto-merges). Replaces the retired /delegate skill.
---

# /implement — the implementation skill

The single path from an issue to an open PR. Pairs with `/scope` and `/approve`: where those produce a vetted plan, `/implement` carries it out in a worktree, loops the Execute⇄Refine cycle internally until CI is green, then holds for merge.

Two entry shapes, one skill:

- **Scoped** — `/implement <issue>` — the issue passed `/scope` + `/approve` (`phase:ready`). The default release-flow path.
- **Quick** — `/implement <issue> --quick` — an ad-hoc fix whose issue body already carries a complete, mechanical fix. Skips the `phase:ready` approval gate and goes straight to Executing. This **replaces the retired `/delegate` skill** — same niche (small, mechanical, the body carries the fix), and runs in the main session (a `--quick` fix is too small to be worth a worktree hand-off; the hybrid background-agent split below is the sanctioned way to delegate the scoped path — see `feedback_delegate_implementation_stop_after_commit`).

Two ways to run it:

- **In-session (default).** The whole skill runs in the main session — implement, push, drive CI green, hold the draft. Use this for a single issue or when you want tight control over each step.
- **Hybrid background-agent.** To parallelize across independent issues, the orchestrator may dispatch one background Agent per issue that does *only* the bounded, parallelizable part: cut the worktree off `main`, implement the plan, commit, then run the full-workspace validation in the foreground — then **STOP**. Committing before validation makes the commit — the durable artifact the parent's takeover adopts — land within the agent's turn regardless of the semaphore-contended validation that is the actual source of turn exhaustion, so an agent that exhausts its turn mid-validation leaves committed, reviewable work. A foreground validation pass (not `run_in_background`) cannot outlive the agent's turn as a detached cargo process holding an `aether-heavy` slot. A validation failure found while the agent is still in budget is fixed by amending the commit; if the agent exhausts its turn before validation completes, the parent's authoritative `scripts/preflight.sh` and the CI Refine loop catch it. The main session ("parent") then takes each finished worktree and runs the serial, less-reliable part itself: `scripts/preflight.sh` (which stamps the commit), the push, the draft-PR open, and the CI-green Refine loop — reviewing the agent's diff, reclaiming any validation process the agent left running (killing it to release its `aether-heavy` slot), and running its own `scripts/preflight.sh` as the authoritative gate as it takes over. Never hand the push, PR creation, CI loop, or phase-label writes to the dispatched Agent: handing off the *whole* skill (the retired `/delegate`) proved flaky, so the split keeps the unreliable parts in-session (see `feedback_delegate_implementation_stop_after_commit`). **The parent owns the `phase:executing` flip at dispatch:** before or as it spawns the agents, the parent performs §Execute step 1 — the `phase:executing` label reconcile — for every issue in the batch. The dispatched agent never touches the phase label, so the flip cannot be deferred to it; leaving it for the parent's takeover would hold each issue at `phase:ready` for the whole time its agent is implementing, misreporting the in-flight fleet and inviting a double-dispatch. `Executing` here means *handed to an agent*: with batched per-agent queues every issue in a queue flips at dispatch, so a queued-but-not-yet-started issue reads `phase:executing` slightly early — accepted, because the parent cannot observe intra-agent queue progress and `phase:ready` would misreport the whole window.

  **Disk use.** Each worktree's `target/` is private to that worktree — no `CARGO_TARGET_DIR` override is set and none should ever be introduced on the default path, as a shared target across worktrees with divergent source poisons incremental checks and directly conflicts with #2202. Once the parent's push succeeds in the serial tail (work fully captured in the draft PR), reclaim the validation scratch with `rm -rf .claude/worktrees/issue-<N>/target` — the target is reproducible at any time from the committed source, so deleting it after push (not after the agent's commit, so the parent's `scripts/preflight.sh` build isn't thrown away) has no lasting cost while bounding a queue's peak footprint to roughly one live `target/` at a time.

  **Batched dispatch.** Before spawning agents, the orchestrator reads each candidate issue's `size:*` and `model:*` labels over REST (`gh api repos/iamacoffeepot/aether/issues/<n> --jq '.labels[].name'` per issue, or one `gh api 'repos/iamacoffeepot/aether/issues?labels=size:m&state=open' --jq '.[].number'` sweep — not `gh issue view` / `gh issue list`, which are GraphQL-backed) — no GraphQL query needed, since `/scope` stamps the size and model routing onto labels at Plan time. It then packs the approved issues into per-agent queues by an **estimated context budget** rather than a fixed count: a queue accumulates issues until the next one would push it past the ~150k-token compaction threshold an agent hits, then a new queue opens.

  The `size:*` label is the prior context-cost estimate — heuristic anchors **S ≈ 25k, M ≈ 60k, L ≈ 120k** accumulated agent context (exploration + diff + validation churn) — and reading each candidate's body and `## Implementation plan` refines it: step count, the count of files and crates the plan touches, and how much exploration the change implies all move the estimate off its label anchor. Pack greedily against the refined estimates: smalls pack densely (several trivial S can share one agent where the old count rule capped at three), mediums co-queue when two fit under the cap, and an L stays solo because its prior alone approaches the threshold.

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

3. **Pack and order.** Apply the **Batched dispatch** rules above (under the hybrid background-agent mode): budget-based packing against the `size:*`-label priors refined by each body read, crate-affinity co-queueing with the trivial-mechanical exception, broadest-exploration-first ordering within each queue. Concurrency equals the number of packed queues, bounded by the ~150k context-budget packing threshold (§Batched dispatch), not a flat agent count — the binding axis is per-agent context, not the REST pool.

   **Stale-worktree probe.** A re-swept issue from a prior bounced or aborted attempt can leave a stale `.claude/worktrees/issue-<N>` worktree and branch behind, so probe each packed candidate before dispatch: does the worktree exist, how many files are uncommitted in it, is its branch ahead of `origin/main`, and is there an open PR for the head branch (the REST `pulls?head=` form, never `gh pr list`):

   ```bash
   wt=.claude/worktrees/issue-<N>; br=<type>/issue-<N>-<slug>
   dirty=$(git -C "$wt" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
   ahead=$(git -C "$wt" rev-list --count origin/main..HEAD 2>/dev/null)
   pr=$(gh api "repos/iamacoffeepot/aether/pulls?head=iamacoffeepot:$br&state=open" --jq '.[].number')
   ```

   Classify each: **safe to auto-clear** when the worktree is clean (`dirty == 0`), its branch is not ahead of `origin/main` (`ahead == 0`), and there is no open PR — clear it at dispatch with `git worktree remove "$wt"` plus `git branch -D "$br"`. **Flag** when any of uncommitted files, unpushed commits, or an open PR is present — clearing would discard bounce context or unpushed work, so surface it as a plan line item rather than clearing, and let the one confirmation prompt the sweep already prints cover the destructive decision.

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

   Dropped:
     #1620  Phase=Design, not Ready
     #1622  no ## Implementation plan
     #1607  umbrella (has ## Sub-issues)

   Confirm dispatch? (the agents spawn only on your go-ahead)
   ```

   Candidates with no stale worktree need no line. Omit the **Stale worktrees** block entirely when none of the dispatched candidates have one.

5. **On confirmation, dispatch.** Clear the stale worktrees first: the auto-clear set unconditionally, and any flagged set the user confirmed (`git worktree remove` plus `git branch -D` per candidate) so each agent's `git worktree add` starts clean.

   **Free-space guard.** Before spawning each agent and before each next-issue build inside a queue, check available disk space on the worktree volume (e.g. `df -k .claude/worktrees` → available column). If free space is below the headroom floor — default 20 GiB, overridable as a knob — pause and surface a plan line ("paused: `<X>` GiB free on worktree volume, below 20 GiB floor — reclaim space or raise the floor before resuming") rather than dispatching. This converts the cross-filesystem-spill failure (ENOSPC → agent relocates `target/` onto another volume and exhausts that one too) into a clean, observable stop.

   Then, for every issue in every queue, the **parent** performs §Execute step 1 (the `phase:executing` label reconcile) at dispatch time — see the hybrid background-agent paragraph — and spawns one background agent per queue, each working its queue's issues in order as full single-issue `/implement` runs that stop after commit. The parent then takes over each finished worktree per the hybrid split (preflight, push, draft PR, Refine loop). After each push succeeds, reclaim the validated worktree's scratch with `rm -rf .claude/worktrees/issue-<N>/target` (see the Disk use note in the hybrid background-agent split above). Each worktree keeps its own private `target/` — no `CARGO_TARGET_DIR` override — per #2202.

The sweep never auto-confirms and never dispatches the serial tail (push / PR / CI loop / phase-label writes) to an agent — it only assembles and confirms the batch the hybrid mode then runs.

## Invocation

```
/implement <issue>                       scoped run (defaults: retry-cap=3, wall=30min)
/implement --sweep                       enumerate every phase:ready issue, pack per-agent queues, confirm, dispatch
/implement <issue> --quick               ad-hoc fix: skip the phase:ready gate (body must carry a complete fix)
/implement <issue> --attest              validate via attest.sh --publish; heavy CI skips, `verify` gates (see Attested path); resolves to preflight.sh when diff has no heavy-CI surface
/implement <issue> --retry-cap <N>       override retry cap
/implement <issue> --wall-clock <mins>   override wall-clock budget
/implement <issue> --resume              continue an in-flight execution (rare)
```

`--sweep` takes no issue argument — it discovers them. It is the batched hybrid background-agent entry point: one REST enumeration of the eligible set, budget-based packing into per-agent queues, a confirmation gate, then dispatch. See [Sweep dispatch](#sweep-dispatch).

## Preconditions

| Check | Refusal |
|-------|---------|
| `phase:ready` label present | "Issue is not Ready (no `phase:ready` label). Use `/scope` + `/approve` first." |
| Exactly one `model:*` label | "Missing model:* label (or more than one). Re-run `/scope`'s Plan step or stamp the label by hand." |
| §Sub-issues section absent or empty | "Issue is an umbrella with sub-issues. Delegate the children, not the parent." (The malformed-umbrella case — a non-empty `## Sub-issues` alongside a substantial own plan — is refused upstream at `/approve`'s Umbrella integrity gate, so any issue that reaches `/implement` with a non-empty `## Sub-issues` is a pure umbrella and correct to drop.) |
| Issue body has `## Implementation plan` | "Missing implementation plan — issue isn't fully scoped. Re-run `/scope`." |
| `gh auth status` has `repo` scope | "Run `gh auth refresh` (repo scope is standard)." |

**`--quick` mode relaxes the gate.** With `--quick`, the `phase:ready` and `model:*`-label checks are skipped (a `--quick` fix runs in the main session — no agent is dispatched, so there is nothing to route). In exchange, the issue body MUST carry a complete, mechanical fix — either a `## Implementation plan` section or an unambiguous proposed-fix description. Before proceeding, sanity-check the body:

- **Body ambiguous or missing the fix** → refuse: *"`--quick` needs a complete fix in the body. Run `/scope <issue>` to design it."* Don't guess.
- **Fix looks design-bearing** (new public API, wire-format change, ADR-worthy choice) → refuse: *"This needs design, not a quick fix. Run `/scope <issue>`."* `--quick` is for mechanical work only (the old `/delegate` bar).

**`--attest` adds its own preconditions** — enforced at the validation step and only when the committed diff has heavy-CI surface (see [Attested path (`--attest`)](#attested-path---attest) §Surface gate); refused with no silent fallback to the unattested path when violated. The flag is orthogonal to `--quick` and applies only to the in-session serial tail.

## Attested path (`--attest`)

`--attest` swaps `/implement`'s local validation from `scripts/preflight.sh` to `scripts/attest.sh --publish`, opting the PR into the **attested CI path**: a write-collaborator's signed local run of the canonical checks stands in for the runner re-running the heavy jobs. `attest.sh` runs the same canonical check set (`scripts/checks.sh`) as preflight under `witness`, signs each step with the author's GitHub-registered key, and publishes `refs/attestations/<head-sha>`. `ci.yml`'s `changes` job detects that ref and **skips** the heavy jobs (clippy, docs, test, desktop, qodana); the separate `verify` required check (`attest-verify.yml`) validates the signed proof and gates the merge in their place. The path is opt-in per PR by design — default `/implement` (no flag) is unchanged.

**Surface gate.** `--attest` earns its keep only when the committed diff touches surface that triggers the heavy CI jobs — surface that `ci.yml`'s `changes` job (`code` + `qodana` filters) and `attest-verify.yml`'s `code` filter both gate on. Classify the committed diff at the validation step:

```sh
git diff --name-only origin/main...HEAD \
  | grep -Eq '^(crates/|Cargo\.toml$|Cargo\.lock$|rust-toolchain\.toml$|\.github/workflows/|qodana\.yaml$)'
```

- **Match (heavy-CI surface present):** enforce the preconditions below and run the attested path exactly as described — `attest.sh --publish`, ref publish, `verify`-green done-condition.
- **No match (no heavy-CI surface):** the heavy jobs skip in `ci.yml` on the path filter and `verify` passes through in `attest-verify.yml` with or without a ref — so `--attest` resolves to the **plain path**: validation runs `scripts/preflight.sh` (which itself short-circuits for docs / CI-config diffs), no `attest.sh`, no ref publish, and the done-condition reverts to plain `CI pass` green. The preconditions below are **not** enforced on this path. The downgrade is announced, not silent — it is an intentional surface-driven resolution, distinct from the banned silent fallback on an attest *failure*.

The surface predicate mirrors `ci.yml`'s `changes` filters — keep in lockstep if those filters change.

**Preconditions** (enforced at the validation step when the diff has heavy-CI surface; skipped on the no-surface plain path; refuse with no silent fallback to the unattested path when violated on a surface-bearing diff):

| Check | Refusal |
|-------|---------|
| `witness` on PATH | "`--attest` needs `witness` (go install github.com/in-toto/witness@latest)." |
| `sshpk-conv` on PATH | "`--attest` needs `sshpk-conv` (npm i -g sshpk)." |
| Docker available (attest runs qodana) | "`--attest` runs qodana, which needs Docker. Start Docker or drop `--attest`." |
| An unencrypted ed25519 signing key (`AETHER_ATTEST_KEY` or `~/.ssh/id_ed25519`) whose public key is registered on the author's GitHub account | "`--attest` signing key not found / not registered on GitHub. attest signs with a key already on your account so `verify` can resolve it." |
| Author is a write-collaborator on the repo | "`--attest` is collaborator-only — only a write-collaborator can publish the attestation ref and have `verify` accept it." |

**Where it changes the flow** (everything else is the normal in-session run; all bullets apply only when the surface gate classifies the diff as heavy-CI surface — the no-surface branch runs `scripts/preflight.sh` with none of these changes):

- **Validation (Execute step 3):** run `scripts/attest.sh --publish` against the committed HEAD instead of `scripts/preflight.sh`. attest writes the same `.git/aether-preflight-passed` stamp on success, so the branch push in step 4 passes the pre-push gate unchanged.
- **Done-condition (Refine loop):** `CI pass` going green is necessary but not sufficient — also require the `verify` check-run `success` over REST (the heavy jobs skip, so `CI pass` is green on skipped results while `verify` is the real gate `wave-status.sh` does not watch). On the no-surface plain path, `CI pass` alone is the done-condition.
- **Re-attest on each fix push:** every fix is a new head sha — re-evaluate the surface gate, then re-run `attest.sh --publish` when surface is present (skipping it degrades gracefully — the heavy CI jobs run for that sha); a fix that newly touches code re-engages the attested path, one that removes all code surface reverts to the plain path.
- **Qodana is local, not deferred:** attest runs and attests qodana, so the "sole `Qodana scan` red held for `/land`" branch does not apply — qodana is covered by the attestation and gated by `verify`.

**Serial-tail only.** `--attest` lives in the in-session serial tail (where preflight, push, and the PR open already live) — it is **never** handed to a dispatched background agent, which must not push the attestation ref, consistent with the rule that push / PR / CI stay in-session. In hybrid background-agent / `--sweep` runs the parent owns the attest-and-publish step the same way it owns preflight and the push; the parent applies the surface gate per worktree, so surface-free worktrees run `scripts/preflight.sh` while surface-bearing ones run `attest.sh --publish` — the preconditions are checked once before the first real attest, not per-worktree. Across a sweep the parent may run the surface-bearing `attest.sh --publish` calls **in parallel**, bounded only by the `aether-heavy` N=2 cargo semaphore; no qodana-singleton serialization is needed because `attest.sh` exports a unique `QODANA_CLI_CONTAINER_NAME` per run (derived from the `mktemp`-unique clone dir) and retries the rare `Only one instance of Qodana` transient exactly once at the witness-invocation level.

## Worktree setup

```bash
# branch name derived from issue: <type>/issue-<N>-<slug>
git worktree add .claude/worktrees/issue-<N> -b <type>/issue-<N>-<slug> main
cd .claude/worktrees/issue-<N>
```

Worktree path is `.claude/worktrees/issue-<N>` (gitignored per CLAUDE.md §Workflow) so concurrent `/implement` runs on different issues don't collide. Branch is cut from `main` (not the current branch) per the user's memory rule.

Before the `git worktree add`, run the same [stale-worktree probe](#sweep-dispatch) §Sweep dispatch uses, for this one issue: if `.claude/worktrees/issue-<N>` already exists from a prior aborted or bounced attempt, check its uncommitted-file count, whether its branch is ahead of `origin/main`, and whether an open PR is attached (the REST `pulls?head=` form). Auto-clear when safe — clean worktree, branch not ahead, no open PR — with `git worktree remove` plus `git branch -D`, then proceed with the add. Surface and stop when the worktree is dirty, ahead, or PR-attached: clearing would discard uncommitted bounce context or unpushed work, so report the state and let the user decide rather than forcing the add.

Ground every read against the current ref per `/scope`'s canonical [Grounding against `origin/main`](../scope/SKILL.md#grounding-against-originmain) section — fast-forward to `origin/main` before reading, verify `HEAD == origin/main`, and treat a surprise call site as a staleness smell to diff before escalating. The worktree this skill cuts is branched from `main`, and a per-agent tree can be cut before a sibling PR lands, so without this an implement run grounds its work against code that has already changed on main.

Type comes from the issue's `type:*` label. Slug is the issue title sanitized: lowercased, alnum + dashes, max 30 chars.

## Execute phase

1. Reconcile the issue label to `phase:executing` (see [Phase label reconcile](#phase-label-reconcile)) — the canonical phase write. No comment — the label event records the transition, and the branch/worktree are printed in the run's output. In hybrid background-agent mode the **parent** performs this step at dispatch time for every issue in the batch (see the hybrid background-agent paragraph above); the dispatched agent begins at step 2.

2. Implement per the issue body's `## Implementation plan` section. The agent follows the plan literally: same files, same sequence, same test coverage. Deviations are bounces, not freelancing.

3. Run local pre-flight before pushing (keep this list in lockstep with `scripts/checks.sh` — that file is the canonical source). In hybrid background-agent mode the agent has already committed before reaching this step (per the Hybrid paragraph above) and runs this list in the foreground; completing it is not a precondition for the parent's takeover handoff — the commit already lands the durable artifact. The in-session path is unaffected: it commits at push time in step 4.
   - `cargo fmt`
   - `cargo clean -p <crate>` for each crate the diff touches (ensures the final clippy below runs against a non-incremental target and cannot inherit a stale cache that masks a warning, e.g. an unused import left after a move)
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo nextest run --workspace`
   - `RUSTDOCFLAGS="-D rustdoc::redundant_explicit_links -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links" cargo doc --workspace --no-deps`
   - wasm32 cross-build for component crates (`cargo metadata` → packages with `crate-type = cdylib` and `aether-actor` dep)
   - `scripts/preflight.sh` if present (writes the stamp file expected by the pre-push hook). Qodana is **not** run here — it is a required CI gate (`Qodana scan`, in `ci-pass`) resolved at `/land` from the `qodana-report` artifact, so a sole `Qodana scan` red holds the draft for `/land` rather than blocking the push (see the Refine loop's CI-green branch)

   **Under `--attest`**, first evaluate the surface gate (see [Attested path (`--attest`)](#attested-path---attest) §Surface gate): when the committed diff has heavy-CI surface, run `scripts/attest.sh --publish` against the committed HEAD *instead of* `scripts/preflight.sh` — attest is a strict superset (the same canonical checks plus the git attestor, signing, and the `refs/attestations/<sha>` publish that opts the PR into the attested CI path), so it is one or the other, never both; attest writes the same pre-flight stamp on success, so the branch push in step 4 passes the pre-push gate as usual; qodana **is** run and attested (needs Docker), so the "sole `Qodana scan` red held for `/land`" branch below does not apply on this path. When the diff has **no heavy-CI surface**, announce the downgrade and run `scripts/preflight.sh` instead — the attested path is skipped and no ref is published.

4. Push the branch, then open the PR over REST (`gh pr create` is GraphQL-backed; `POST …/pulls` is REST and takes `draft: true` directly). Write the PR body to a file first so backticks / `$` in the template aren't shell-expanded, and pass it with `-F body=@<file>`:
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

   `wave-status.sh --wait <pr>` loops (polling every 20s) until `CI pass` — the required merge aggregator — is present and completed with zero pending check-runs, then exits 0 on `success` or 1 on failure/neutral. A subset-registered matrix (only `Detect changes` up, say) can't trip a false green. Exit 0 → goto step 2; exit 1 → the script has already printed the failed child check names — go to step 3.

2. **CI green** → goto "Done condition" below. **Or green except a sole `Qodana scan` red** — the failing required checks minus `CI pass` are exactly `{Qodana scan}`: also goto "Done condition". A sole Qodana red is not fixed or Stalled here; it is held at `phase:refine` for `/land` to resolve from the `qodana-report` artifact. (Any other red alongside it is a real failure — go to step 3.)

   **Under `--attest` when the diff has heavy-CI surface**, `wave-status.sh --wait` watching `CI pass` is not sufficient: the heavy jobs *skip*, so `CI pass` goes green on skipped results while the real gate is the separate `verify` required check (`attest-verify.yml`), which `wave-status.sh` does not watch. After `wave-status.sh --wait` returns 0, also confirm the `verify` check-run is `success` over REST (`repos/iamacoffeepot/aether/commits/<head-sha>/check-runs`, select `.name == "verify"`); treat green only when both are satisfied. The qodana-only-red branch does not apply on this path (qodana is attested locally). A red `verify` is a real failure — go to step 3. When the diff has no heavy-CI surface, `--attest` resolves to the plain path and `CI pass` alone is sufficient.

3. **CI failed** (a non-Qodana required check is red) → pull logs (`gh run view <run-id> --log-failed`), classify, act:

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

4. If real failure, fix in the worktree, push to the same branch, increment attempt counter, goto step 1. **Under `--attest`**, re-evaluate the surface gate before the push: when the diff has heavy-CI surface, re-run `scripts/attest.sh --publish` to re-validate and re-publish the attestation ref against the new sha (skipping it degrades gracefully — the heavy CI jobs run for that sha); when the diff has no heavy-CI surface, run `scripts/preflight.sh` instead. A fix that newly touches code re-engages the attested path; a fix that removes all code surface reverts to the plain path.

5. Swap the `phase:*` label to `phase:refine` during fix-and-wait, back to `phase:executing` when pushing the fix (see [Phase label reconcile](#phase-label-reconcile)). (Flicker is intentional — gives honest visibility into the in-flight state.) No per-attempt comments — the PR's own commit and check history is the attempt record; track the attempt counter in-session.

6. **Retry cap hit** → self-bounce. `phase:bounced`, `bounce-to:plan` label, comment with the full attempt history.

7. **Wall-clock hit** → self-bounce. Same as retry cap with the elapsed time noted.

8. **Design-level discovery** at any attempt → self-bounce. `phase:bounced`, `bounce-to:design` label, comment with the specific finding. Examples:
   - "Approach X doesn't work because Y; needs alternative."
   - "Test Z passes only if we also change A, which is outside §Implementation plan."

## Flake detection (v1, simple)

Per-test counter. If test `foo::bar` fails on attempt 1, store it. If it fails again on attempt 2, real failure — fix the underlying cause. If different tests fail each attempt with no common cause, treat as flake — rerun CI (no push) up to 2 times before counting against retry budget.

Format/clippy/build are never flakes — always real, always immediate fix.

## Done condition

CI green — or green except a sole `Qodana scan` red held for `/land`:

1. Phase label: leave the issue at `phase:refine`. The resting state is "draft PR open + green (or Qodana-only red)" — no comment; the PR's checks are the record. A held `Qodana scan` red is normal here: `/land` runs the Qodana sweep before it un-drafts.
2. Leave the PR as a **draft**. Do not un-draft, do not merge, do not close, do not delete the `phase:*` label (Done is a `/land`-time action). Un-drafting is the user's (or the approved release process's) action — once a PR is un-drafted, native auto-merge lands it on green ([[feedback_green_pr_automerges_before_review]]).
3. Print to user:

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

The single `PUT …/labels` replaces the label set with the non-`phase:*` labels plus the one new `phase:*`, so the issue never carries two phase labels and never carries zero — a tighter guarantee than a remove-then-add pair, which has a window between its two calls. A failed PUT leaves the prior labels untouched and heals on the next run. This skill writes the phase label in four places: `phase:executing` (Execute step 1 + Refine-loop fix-push), `phase:refine` (Refine-loop fix-and-wait + done condition), `phase:bounced` (self-bounce on retry-cap / wall-clock / design discovery), and `phase:stalled` (build-env failure). `Done` carries no label — the merge that closes the issue retires the lifecycle (`/land` deletes the label).

## Failure modes

- **PR creation fails** (e.g. duplicate branch from prior aborted run): clean up the stale branch (`git branch -D`), retry. If repeated failure, self-bounce to Plan.
- **Pre-flight CI failure on first push** (formatting, build): fix in-worktree and push. Doesn't count against retry budget — local-equivalent failures are pre-CI.
- **Stale worktree from a prior aborted or bounced run** (`.claude/worktrees/issue-<N>` already exists): the [stale-worktree probe](#sweep-dispatch) catches this before `git worktree add` runs — auto-cleared when safe (clean, branch not ahead, no open PR), surfaced for a decision when dirty / ahead / PR-attached — both in §Sweep dispatch for the batch and inline in §Worktree setup for a single-issue run. If `git worktree list` is itself wedged so the remove can't proceed, instruct the user to clean it up manually.
- **Phase regression while running** (someone hand-bounces the issue mid-execution): detect on the next phase-label swap, abort the loop, leave the branch and PR as-is, post a comment noting the abort.
- **PR gets reviewer comments mid-loop**: ignore in v1. `/implement` only listens to CI signal. Reviewer feedback is a separate human concern — they can `/bounce` or comment on the PR directly.

## What `/implement` does NOT do

- Merge the PR (manual or Phase C orchestrator).
- Edit the issue body (only `/scope` does).
- Re-scope the issue when CI surfaces problems — bounce instead.
- Address reviewer feedback on the PR. Reviewers comment; `/bounce` if the feedback requires re-scoping; manual handling otherwise.
- Notify anyone. The printed output and the `phase:*` labels are the surface; the only comment this skill posts is a self-bounce reason.
- Merge — code PRs always hold for your review; auto-merge is the release process's call, not this skill's.
- Run scoped (without `--quick`) on an issue that isn't at `phase:ready`. For an ad-hoc fix whose body already carries the change, use `--quick`.
- Clean up worktrees after success or bounce. Leaves them for inspection; `git worktree remove .claude/worktrees/issue-<N>` is the user's call (`/sweep worktrees` automates this at merge). Exception: in `--sweep` / hybrid background-agent runs the parent reclaims the bulky `target/` subdir immediately after each push succeeds (`rm -rf .claude/worktrees/issue-<N>/target`), while keeping the (small) source tree for inspection until `/sweep` removes the whole tree at merge. This bounds a queue's accumulated disk footprint without waiting for the user to run `/sweep`.
