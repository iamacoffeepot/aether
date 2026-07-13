---
name: implement-headless
description: Headless variant of /implement for a one-shot GitHub Actions runner. Executes ../implement/SKILL.md verbatim for the plan execution and CI Refine loop; overrides only the interaction surface — the checkout is the workspace, the terminal hold becomes end-turn, commits assert the bot identity — per ../headless/protocol.md.
---

# /implement-headless — headless implementation wrapper

This wraps `/implement` for a headless agent running one-shot on an ephemeral GitHub Actions runner. It carries no process of its own.

Execute `../implement/SKILL.md` verbatim — the preconditions, the plan-literal Execute phase, the commit-and-format step, the push, the draft-PR open, the CI Refine loop, the phase-label reconcile, the self-bounce mechanics. Where the original touches the interaction surface, `../headless/protocol.md` governs. An instruction is overridden if and only if it appears in the Overrides table below, cited by the original's anchor; everything else is the original's, unchanged. An improvement to `/implement` is live here the moment it merges, because this wrapper only references it.

Before any process step, run the protocol's [re-entrancy-first](../headless/protocol.md#re-entrancy-first) guard: check whether the PR is already merged, whether the branch already exists, which `phase:*` label is present, and whether an unanswered `agent:awaiting-answer` park is open, then post a start-of-work comment with the run link and begin the original at the phase the observed state implies.

## Overrides

| Original anchor | Interactive behavior | Headless override |
|-----------------|---------------------|-------------------|
| [`## Worktree setup`](../implement/SKILL.md#worktree-setup) | `git worktree add "$main_root/.claude/worktrees/issue-<N>"`; the stale-worktree probe surfaces and stops when dirty/ahead/PR-attached | [checkout-as-isolation](../headless/protocol.md#checkout-as-isolation) + [re-entrancy-first](../headless/protocol.md#re-entrancy-first) — the runner's `$GITHUB_WORKSPACE` checkout is the workspace; the worktree add is a no-op, and the re-entrancy guard derives prior-attempt state from GitHub rather than from a stale on-disk worktree |
| [`## Execute phase`](../implement/SKILL.md#execute-phase) step 3 commit | Commit the work in the current git identity | [commit-identity-assert](../headless/protocol.md#commit-identity-assert) — assert the owner's public `user.name` / `user.email` before committing; never author under the owner's real name or personal email |
| [`## Self-bounce mechanics`](../implement/SKILL.md#self-bounce-mechanics) human-addressed bounce comment | Prose bounce comment carrying the reason and attempt history | [ask-and-park](../headless/protocol.md#ask-and-park) — when the bounce needs an owner decision, post the same content in the park shape, apply `agent:awaiting-answer` + `agent:park:implement`, exit 0 |
| [`## Done condition`](../implement/SKILL.md#done-condition) "Print to user" and the "hold the draft, tell me to land" terminal | Print the draft-PR summary and wait for the operator to un-draft or say land | [end-turn-not-wait](../headless/protocol.md#end-turn-not-wait) — leave the PR a held green draft (the reconciler computes its resting state — `phase:held`, or `phase:findings` when the rollup is actionable), post the terminal state as a comment, end the turn; re-dispatch resumes |

**Not overridden — the Refine loop's CI wait.** [`## Refine loop`](../implement/SKILL.md#refine-loop-the-spin-until-green-part)'s `scripts/wave-status.sh --wait <pr>` poll is an in-job wait on CI, not on a human, so [end-turn-not-wait](../headless/protocol.md#end-turn-not-wait) does not override the wait itself — but a one-shot runner cannot block a foreground on it, so the agent realizes it the headless way the protocol's [CI-wait contract](../headless/protocol.md#the-ci-wait-contract) specifies: poll it mechanically with cheap in-turn status-check turns until CI settles, then continue the loop in the same box — never ending the turn on a pending background wait, and — while the wait is unsettled — never `ScheduleWakeup stop` or sign off as if the turn were done, either of which releases the process and abandons the loop before CI settles. How the reconciler and the in-job loop divide the building→held stretch is the executor workflow's call, out of scope for this wrapper.

Everything the table does not cite — the preconditions, the plan-literal execution, the commit-and-format discipline, the push, the draft-PR open, the CI classification and flake handling, the phase-label reconcile — is `/implement`'s, verbatim.
