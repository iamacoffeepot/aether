---
name: sweep
description: Reclaim stale local state. `/sweep` (or `/sweep worktrees`) removes worktrees + branches whose PRs merged; `/sweep branches` prunes worktree-less local branches whose PRs merged; `/sweep memory` compresses + de-indexes the project's stale memory index; `/sweep adrs` flags ADRs whose recorded status has drifted from reality (Proposed-but-shipped, unacknowledged supersession); `/sweep fat` decomposes fat (`size:xl`-labelled) issues into skinny children and closes-and-replaces the parent — recursively until every leaf is skinny; `/sweep all` runs every target except `fat`. Each enumerates candidates, classifies by a staleness signal, prints a plan, and confirms before acting. Pair the git targets with `/implement` — after implemented PRs land, run `/sweep` to reclaim disk + branch space.
---

# Sweep skill

Reclaims accumulated local cruft. Every target runs the same five beats — **enumerate candidates → classify each by a staleness signal → surface a plan → confirm → act → report** — and differs only in the *signal* (what marks an entry stale) and the *action* (how it's removed).

## Targets

Parse the invocation argument to pick the target. Bare `/sweep` defaults to `worktrees` (unchanged behaviour).

| Arg | Sweeps | Staleness signal | Auto-removes |
|-----|--------|------------------|--------------|
| `worktrees` (default) | non-primary git worktrees + their branches | branch's PR merged on GitHub | merged only |
| `branches` | local branches **without** a worktree | PR merged / `[gone]` upstream / merged into main | PR-merged only |
| `memory` | the project's `MEMORY.md` index | over-limit / over-long lines / superseded / orphaned / stale refs | nothing (compress + de-index only) |
| `adrs` | the `docs/adr/NNNN-*.md` decision records | recorded status drifted from reality: Proposed-but-shipped / unacknowledged supersession / stale partial-phase | nothing (surface only) |
| `fat` | open `size:xl`-labelled issues | `size:xl` label present | nothing (decompose + close-and-replace, all confirmed) |
| `all` | run `worktrees`, then `branches`, then `memory`, then `adrs` in sequence (not `fat`) | — | per-target |

The git targets auto-remove only the entries with a hard merge-oracle (a merged PR) after one confirm; everything fuzzier is surfaced and left for the user. The `memory` target has no oracle, so it **never deletes files** — it tightens index hooks and drops index lines for superseded notes (the topic files stay on disk as archive). The `adrs` target **never edits an ADR** — a decision record's status is the user's to change, and the "did it ship" signal is a heuristic, not an oracle; the target only surfaces drift with evidence.

## Target: worktrees (default)

1. **List worktrees.** `git worktree list --porcelain`. Parse blocks of `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` separated by blank lines. Skip the primary worktree (the current working directory) and skip any detached-HEAD worktrees (no `branch` line — nothing to match against).

2. **Match each worktree's branch to PR state over REST.** `gh pr list` is GraphQL-backed and, under GitHub's secondary-rate-limit throttle, *silently returns `[]`* — which this skill would misread as "branch has no PR" and surface for removal of an actually-merged branch (or skip a real one). The REST list errors loudly instead, so a throttle can't masquerade as a clean result. For each non-primary worktree branch:
   - `gh api 'repos/iamacoffeepot/aether/pulls?head=iamacoffeepot:<branch>&state=closed' --jq '[.[] | select(.merged_at != null)][0].number'` — non-empty → `merged: #NNN`. REST has no `merged` state; a merged PR is `state=closed` with `merged_at` non-null.
   - Otherwise `gh api 'repos/iamacoffeepot/aether/pulls?head=iamacoffeepot:<branch>&state=all' --jq '.[0] | .number, .state'` — distinguish `open` / `closed` (closed-not-merged) / no-PR (empty).

3. **Surface the plan.** Print a table of every non-primary worktree: path, branch, status (`merged: #NNN`, `open: #NNN`, `closed-not-merged: #NNN`, `no PR`). Then ask the user to confirm before removing anything.

4. **Remove confirmed entries.**
   - For each `merged` worktree: `git worktree remove <path>` (use `--force` only if the worktree has untracked-but-uncommitted files; the agent should have committed everything, but stragglers happen). Then `git branch -D <branch>` if the local branch still exists.
   - For `closed-not-merged`, `open`, and `no PR`: do not auto-remove. Surface them and let the user decide per-item — those represent abandoned work, in-flight PRs, and possibly in-progress agent runs respectively.

5. **Report what was swept** — paths removed, branches deleted, plus any worktrees left alone with the reason.

## Target: branches

Worktree-less local branches — the ones you checked out and worked on directly, with no dedicated worktree. (Branches that *do* back a worktree are the `worktrees` target's job; skip them here so the two don't double-handle the same branch.)

1. **List local branches with upstream-tracking state.** `git for-each-ref --format='%(refname:short) %(upstream:track)' refs/heads`. Build the worktree-branch set from `git worktree list --porcelain` and subtract it. Always exclude the current branch and the default branch (`main`).

2. **Classify each remaining branch.** Use the REST PR list (`gh pr list` returns `[]` under secondary-throttle and would misclassify a merged branch as no-PR — see the worktrees target):
   - `gh api 'repos/iamacoffeepot/aether/pulls?head=iamacoffeepot:<branch>&state=closed' --jq '[.[] | select(.merged_at != null)][0].number'` non-empty → `merged: #NNN`. This is the hard signal (a squash-merge leaves the branch *not* an ancestor of `main`, so PR state is the only reliable merged signal). REST has no `merged` state — filter `state=closed` on `merged_at != null`.
   - `[gone]` in the upstream-track column → the remote branch was deleted (the usual after a `--delete-branch` merge). Treat as a *merged candidate* but confirm against the PR check before removing.
   - `git branch --merged main` lists it → `merged-into-main` (true-merge ancestry).
   - Open PR (`gh api 'repos/iamacoffeepot/aether/pulls?head=iamacoffeepot:<branch>&state=open' --jq '.[0].number'`) → `open: #NNN` — leave, in flight.
   - No PR and unpushed commits (`%(upstream:track)` shows `ahead` or no upstream + commits not in `main`) → `local-wip` — potential lost work; surface, never auto-remove.

3. **Surface the plan.** Table: branch, status, last-commit date. Confirm before removing.

4. **Remove confirmed entries.**
   - `merged: #NNN` → `git branch -D <branch>` (PR-merge confirmed by GitHub, so force-delete is safe even though squash leaves it non-ancestor).
   - `merged-into-main` → `git branch -d <branch>` (let git's ancestry check be the backstop).
   - `[gone]`-only without a confirmed merged PR, `open`, `local-wip` → do not auto-remove; surface for a per-item decision.

5. **Report** — branches deleted, branches left alone with the reason. Optionally `git fetch --prune` to clear stale `origin/<branch>` tracking refs.

## Target: memory

Curates the current project's auto-memory index (`MEMORY.md`) without losing knowledge. Compress + de-index only — **never delete topic files**.

1. **Locate the memory dir.** `~/.claude/projects/<slug>/memory/`, where `<slug>` is the project's absolute path with every `/` replaced by `-` (e.g. `/Users/x/workspace/aether` → `-Users-x-workspace-aether`). Compute it: `slug=$(echo "$PWD" | sed 's#/#-#g')`.

2. **Measure + enumerate.**
   - Index size: `wc -c MEMORY.md` against the ~24.4 KB (≈24985-byte) limit. Note the margin.
   - Over-long index lines: `awk 'length>200'` — each should be a tight one-liner.
   - Topic files vs index: `ls *.md` minus the entries linked from `MEMORY.md` → **orphaned files** (on disk, never recalled).

3. **Classify each index entry.**
   - **Oversized** (line >~200 chars) → compress the hook (the text after the em-dash), keeping load-bearing identifiers (ADR / PR / issue numbers, crate / mailbox / API names — that's the searchable signal). This is the primary lever; it's lossless.
   - **Superseded** — the body or hook says `superseded` / `historical` / `retired`, or another note explicitly supersedes it (e.g. "Supersedes the N notes below") → **de-index candidate**.
   - **Stale reference** — the entry names a file / crate / symbol / flag / mailbox. Grep the repo to confirm it still exists; if it's been renamed or removed, the entry is stale → surface for the user (a rename-fix or a removal, their call — do not auto-act).
   - **Duplicate** — two entries cover the same fact → consolidation candidate (surface).

4. **Surface the plan.** Per-entry: proposed action (`compress` / `de-index` / `flag-stale` / `leave`) and the resulting projected index size + margin. Before listing a de-index, grep the other topic files for inbound `[[name]]` links to it — dangling links are allowed, but note which retained notes will point at a de-indexed (archived) file. Confirm.

5. **Act.**
   - Compress oversized hooks in place (Edit).
   - For confirmed-superseded entries: remove the index line from `MEMORY.md` but **leave the topic file on disk** — it becomes archive, and any inbound `[[links]]` still resolve to a real file. Re-checking the limit after compression often clears it without any de-indexing.
   - Stale-reference + duplicate entries: surface only; let the user choose fix-vs-remove.

6. **Report** — new index size + margin, count of lines compressed, entries de-indexed (with their archived filenames), and stale refs flagged for follow-up.

## Target: adrs

Flags ADRs in `docs/adr/NNNN-*.md` whose recorded **Status** line has drifted from reality — a decision that shipped but still reads `Proposed`, or a supersession that one side never acknowledged. Surface-only: **never edit an ADR.** The status of a decision record is the user's call, and the shipped-signal is a heuristic (code references), not a hard oracle like a merged PR.

1. **Enumerate ADRs + their status.** `ls docs/adr/[0-9]*.md` (skip `TEMPLATE.md`). For each, read the `- **Status:** …` line near the top. Normalise the leading word: `Proposed` / `Accepted` / `Superseded` / `Rejected`, plus any parenthetical qualifier (`(parked)`, `(Draft …)`, `(Phase 1 only; Phase 2 deferred)`, `(phases 1–3 shipped)`).

2. **Classify each by drift signal.**
   - **Proposed-but-shipped** — status leads with `Proposed` **and** the ADR is cited from non-docs source: `grep -rl "ADR-NNNN" crates --include="*.rs"`. A `Proposed` ADR with a dedicated module / tests citing it (e.g. ADR-0047 → `src/dag/`, ADR-0049 → `src/handle_store/`, ADR-0080 → `src/chassis/settlement*`) has almost certainly been accepted-in-practice; the status line just never caught up. The list of citing files **is** the evidence. Strongest, most common drift.
   - **Supersession asymmetry** — two directions, both drift:
     - *Forward orphan*: ADR-A's status says `Superseded by ADR-B`, but ADR-B never references A (`grep -l "ADR-00A\|0A-" docs/adr/00B-*.md` empty) — the supersession is unacknowledged by the successor.
     - *Backward orphan*: ADR-B's body says it *supersedes / replaces / retires* ADR-A (`grep -il "supersed\|replaces\|retires" docs/adr/*.md` then check which ADR-A it names), but ADR-A's own status still reads `Proposed` / `Accepted` — A's status should be `Superseded by ADR-B`. **This grep is bidirectional and noisy** — a superseded ADR usually points *forward* at its successor near the same words, so the same proximity match fires in both directions. Read the sentence to confirm which ADR is the successor before flagging; never assert direction from the grep alone. (This noise is the reason the target surfaces rather than edits.)
   - **Stale partial-phase** — status like `Accepted (Phase 1 shipped; Phase 2 in progress)` or `(phases 1–2 shipped)`. Check whether the later phase landed (a follow-on `grep` for the phase's subsystem, or a merged PR citing the ADR). If it did, the phase annotation is stale. Lower confidence — surface, don't assert.
   - **Intentionally open (NOT drift)** — status carries an explicit `(parked)` / `(Draft — …)` / `Rejected` qualifier. These are deliberate resting states, not drift. List them in a separate "left as-is (intentional)" group so a genuinely-drifted `Proposed` isn't lost among ADRs that are *meant* to sit at Proposed. A plain `Proposed` with **no** code citations is also not-yet-drifted — a pending decision — leave it.

3. **Surface the plan.** Table per drifted ADR: number, title, current status, **proposed** status, and the one-line evidence (`cited by N source files: …`, `ADR-00B says "supersedes" but A still Accepted`, …). Group by signal; put the highest-confidence (Proposed-but-shipped with many citations, asymmetric supersession) first. Then the "intentional / pending — left as-is" group for completeness. Confirm — but note there is nothing to auto-apply.

4. **Act — surface only.** Print, for each drifted ADR, the exact edit the user could make (`- **Status:** Proposed` → `- **Status:** Accepted (shipped — see crates/…)`), but **do not edit any ADR**. If the user explicitly asks, *then* apply a specific status change as a normal edit (and only the ones they name) — an ADR status change is a decision-record amendment and may warrant its own commit / PR.

5. **Report** — counts per signal (drifted-shipped, asymmetric-supersession, stale-phase), the ADRs flagged, and the intentional/pending set left untouched. No files changed unless the user asked for a named edit in beat 4.

## Target: fat

Decomposes issues labelled `size:xl` — the dreamer's explicit "needs breakdown" stamp (ADR-0110) — into skinny (S/M/L) child issues and closes-and-replaces the fat parent. The pass recurses: a child that itself comes out `size:xl` is re-drilled before the parent is closed. The target terminates when every leaf in the decomposition is skinny. Signal is the `size:xl` label; enumerate uses REST, not the board (the board's `Size` field was removed in the #1825 label-native rollout).

1. **Enumerate candidates.** `gh api 'repos/iamacoffeepot/aether/issues?labels=size:xl&state=open' --jq '.[] | select(has("pull_request")|not) | {number: .number, title: .title}'` — REST-enumerated, off the contended GraphQL pool. Pull requests are excluded by the `select` (PRs appear in the issues endpoint with a `pull_request` key). An empty result is a clean no-op: print "nothing to decompose" and exit.

2. **Classify + drill.** For each fat parent, read its title and body to understand the scope, then decompose it into a set of skinny child issues — each one focused enough to fit in a single PR. A child whose projected scope still reads `size:xl` is itself drilled in a nested pass before it is filed, so the recursion terminates only when every leaf is skinny. Decomposition is agent judgment, not a hard oracle: there is no mechanical rule that derives children from a parent, so the next beat always confirms before filing or closing anything.

3. **Surface the plan.** For each fat parent, print: the parent issue number and title, the proposed child set (one line per child: draft title and projected size — S/M/L), and the close-and-replace action (the parent will be closed with a comment listing its children once all children are filed). Then ask the user to confirm the full plan before any filing or closing fires.

4. **Act on confirmed candidates.**
   - File each confirmed child via `/sketch`'s REST mechanics: `gh api 'repos/iamacoffeepot/aether/issues' --method POST --field title='…' --field body='…'`. Each child body opens with a line linking back to the fat parent (`Decomposed from #NNN.`) and carries a Phase=Backlog annotation so it enters the board at Backlog. Add the child to the release board at Phase=Backlog using the `item_cache` / `project_node_id` from `.claude/release-state.json`.
   - After all children are filed, close-and-replace the fat parent: post a comment on the parent listing the filed child issues (`gh api 'repos/iamacoffeepot/aether/issues/NNN/comments' --method POST --field body='…'`), then close the parent (`gh api 'repos/iamacoffeepot/aether/issues/NNN' --method PATCH --field state=closed`). Archive the parent's project item so only skinny issues stay live on the board.
   - Skip any parent the user declined at the confirm gate; note the reason in the report.

5. **Report** — parents decomposed and closed, children filed (with issue numbers per parent), and any fat issue left un-decomposed with the reason (user declined, or decomposition produced no actionable children).

- **Confirm before acting**, on every target. Auto-removal is limited to the hard-oracle cases (PR-merged worktrees / branches); memory never auto-deletes anything off disk.
- **worktrees**: never sweep the primary worktree or a worktree whose branch has an open PR. `git worktree list --porcelain` blocks are `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` separated by blank lines; detached-HEAD entries lack a `branch` line. A locked worktree refuses removal — `git worktree unlock <path>` first, or surface the lock reason rather than forcing.
- **branches**: never delete the current or default branch, a branch with an open PR, or a worktree-backed branch (handled by `worktrees`). A branch with unpushed commits and no merged PR is potential lost work — surface, never auto-remove.
- **memory**: never delete topic files (de-index keeps them as archive). Don't touch `user`-type memories (identity facts, rarely stale) without an explicit ask. When compressing, preserve the searchable identifiers. Verify a named file/symbol still exists in the repo before calling an entry stale — a memory reflects what was true when written, not necessarily now.
- **fat**: not part of `all` — it is a board operation the dreamer invokes explicitly (ADR-0110's weigh loop), not local-cruft reclaim. Decomposition is judgment, not an oracle, so the target always confirms before filing children or closing any parent; the close-and-replace only fires on the children the user approves. A `/sweep fat` run with no open `size:xl` issues is a clean no-op, not an error. The `size:xl` label is the sole signal — no board field lookup, no `release-state.json` Size read.
- Remote tracking refs are pruned by GitHub when a PR merges with `--delete-branch`. If a stale `origin/<branch>` lingers, `git fetch --prune` clears it — optional final step for the git targets.
