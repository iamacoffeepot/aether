---
name: sweep
description: "Enumerate, classify, confirm, and reclaim stale Aether Claude worktrees, branches, memory index entries, ADR status drift, or fat issues without discarding dirty or live work."
---

# /sweep — evidence-backed cleanup and audits

Every target uses the same contract: enumerate, classify with evidence, print exact proposed actions, end for confirmation, act only on the confirmed set, then report. A failed GitHub read is unknown, never an empty result.

```
/sweep
/sweep worktrees
/sweep sessions
/sweep branches
/sweep memory
/sweep adrs
/sweep fat
/sweep all
```

Resolve the shared repository root from the absolute common Git directory and capture the caller's current worktree. Never remove the main root, current worktree, or uncertain ownership. Managed Claude issue worktrees live under `.claude/worktrees/issue-*`; other branch-backed paths are manual or legacy unless explicitly identified.

## Worktrees

Enumerate `git worktree list --porcelain`. For every managed branch-backed entry record absolute path, branch, clean/dirty state, locks, ahead/behind against fresh main, and matching pull-request state over REST.

Only a clean managed worktree with a GitHub-confirmed merged pull request is a removal candidate. The confirmation plan names worktree removal and local branch deletion. Retain and explain open, closed-not-merged, no-pull-request, dirty, locked, manual, and API-unknown entries. Re-read status and pull-request state immediately before removal. Use `-D` for a squash-merged branch only after the merged oracle is reconfirmed.

## Sessions

Enumerate detached worktrees beneath `.claude/worktrees/` that are not owned issue or ADR worktrees. Cleanliness and age are hints, not liveness proof. Report path, current-worktree status, dirtiness, HEAD reachability from main, commit date, directory time, and registered/prunable state.

Never auto-remove a session worktree or include the current one. Offer clean non-current entries only for exact per-path confirmation with an ownership warning. Dirty entries require separate explicit authority to discard named changes. Use worktree prune only for already-missing administrative entries shown in the plan.

## Branches

Enumerate local branches, subtract worktree-backed branches, and exclude main/current. Fetch/prune only as a disclosed network operation.

Classify with REST pull-request lookup and local ancestry:

- confirmed merged pull request → candidate, delete with `-D` after confirmation;
- open pull request → retain;
- closed not merged → retain;
- gone upstream without confirmed merge → retain;
- no pull request and commits absent from main → retain as local work;
- ancestry-merged into main → candidate, delete with `-d`.

Print upstream, ahead count, last commit, and evidence. Recheck immediately before deletion.

## Memory

Curates the project's auto-memory index without losing knowledge: compress and de-index only, never delete a topic file, and never touch `user`-type memories without an explicit ask. Locate the memory directory at `~/.claude/projects/<slug>/memory/`, where `<slug>` is the project's absolute path with every `/` replaced by `-`.

Measure `MEMORY.md` against its ~24.4 KB index limit and note the margin. Enumerate over-long index lines (over ~200 characters), topic files on disk that no index or sub-index links (orphans), and index links whose target file is missing (dead links).

Classify each index entry: over-long hooks are compression candidates — compression preserves the searchable identifiers (issue, pull-request, and ADR numbers; crate, mailbox, and symbol names) and is the primary, lossless lever; entries whose body or hook says superseded, historical, or retired — or that another note explicitly supersedes — are de-index candidates; entries naming a repository file, symbol, or flag are stale when a grep shows the name renamed or removed (a memory records what was true when written, so surface these for the user's fix-or-remove call); two entries covering the same fact are a consolidation candidate, also surfaced only.

Print per-entry proposed actions (compress, de-index, flag-stale, leave) with the projected index size, note which retained notes hold `[[links]]` into files being de-indexed, and wait for confirmation. De-indexing removes the index line and leaves the topic file on disk as archive, so inbound links still resolve. Report the new index size and margin, lines compressed, entries de-indexed, and stale references flagged.

## ADR audit

This target is read-only unless the user later names exact edits. Enumerate numbered ADRs and surface Proposed documents contradicted by shipped code, asymmetric supersession references, and status text contradicted by current code or merged pull requests. Read relevant sentences; grep is evidence, not proof. Print current/proposed status and exact evidence. Apply only separately approved named edits as ordinary repository work.

## Fat issues

This target reads body artifacts, not labels. Enumerate open non-pull-request issues, parse complete managed Plans with `.agents/skills/approve/scripts/plan_digest.py`, and retain those whose exact `**Size:**` value is `l`.

Classify as fat only when a Plan has more than three separable changes, spans more than two separable crates, or cannot remain one reviewable concept. Verify maintainer intent and code surfaces. Recursively propose focused children until every leaf is skinny. Show exact conventional titles, body outlines, taxonomy, projected body routing, parent link, and parent replacement action; then wait for confirmation.

After confirmation file children sequentially through `/sketch`. Each child starts unscoped with no managed Plan or routing. On any failure stop without closing the parent. Close/replace only after every confirmed child exists, using one human-readable parent summary.

## All

Build one combined plan in order: worktrees, sessions, worktree-less branches, memory, ADR audit. Fat-issue decomposition is excluded. Ask once for itemized confirmation but retain stricter per-item rules. Execute serially and re-enumerate between targets.
