---
name: land-headless
description: Headless variant of /land for a one-shot GitHub Actions runner. Executes ../land/SKILL.md verbatim for the gate checks, conflict oracle, Qodana sweep, and landing sequence; overrides only the interaction surface — dirty-conflict and Qodana bail-outs become ask-and-park, the confirmation wait becomes end-turn, the merge asserts the bot identity — per ../headless/protocol.md.
---

# /land-headless — headless landing wrapper

This wraps `/land` for a headless agent running one-shot on an ephemeral GitHub Actions runner. It carries no process of its own.

Execute `../land/SKILL.md` verbatim — the preconditions and QA-label gate, the conflict prediction oracle, the rebase handling, the Qodana sweep, the un-draft and squash-merge, the phase-label delete. Where the original touches the interaction surface, `../headless/protocol.md` governs. An instruction is overridden if and only if it appears in the Overrides table below, cited by the original's anchor; everything else is the original's, unchanged. An improvement to `/land` is live here the moment it merges, because this wrapper only references it.

Before any process step, run the protocol's [re-entrancy-first](../headless/protocol.md#re-entrancy-first) guard: check whether the PR is already merged (a crashed land job that already merged leaves nothing to redo), whether the closing issue still carries its `phase:*` label, and whether an unanswered `agent:awaiting-answer` park is open, then post a start-of-work comment with the run link and begin the original at the point the observed state implies.

## Overrides

| Original anchor | Interactive behavior | Headless override |
|-----------------|---------------------|-------------------|
| [`## Conflict prediction and routing`](../land/SKILL.md#conflict-prediction-and-routing) Dirty conflict handling | Surface the conflicting files to the user and stop | [ask-and-park](../headless/protocol.md#ask-and-park) — post the conflict as the structured park question with the owner's resolve/delegate options, apply `agent:awaiting-answer`, sync the session to S3, exit 0. Never touch the branch contents |
| [`## Qodana sweep`](../land/SKILL.md#qodana-sweep) "Bail out — surface to the user" | Surface an artifact-missing / outside-the-diff / uncertain Qodana case to the user, do not fix | [ask-and-park](../headless/protocol.md#ask-and-park) — park the bail-out case to the owner rather than auto-suppressing |
| [`## Sweep land`](../land/SKILL.md#sweep-land) "wait for one confirmation" and the [`## Preconditions`](../land/SKILL.md#preconditions) approved-PR expectation | Print the land plan and wait for the operator's one confirmation before merging | [end-turn-not-wait](../headless/protocol.md#end-turn-not-wait) — the dispatch itself is the authorization; the approval policy is the reconciler gate of a later rung. Post the plan as a comment and end the turn rather than blocking on a prompt |
| [`## Landing sequence`](../land/SKILL.md#landing-sequence) step 6 squash-merge | Issue the squash merge with `commit_title` in the current git identity | [bot-identity-assert](../headless/protocol.md#bot-identity-assert) — assert the `aether-agent` bot `user.name` / `user.email` before merging; never author the merge under the owner's identity |
| [`## Landing sequence`](../land/SKILL.md#landing-sequence) step 8 worktree sweep | `git worktree remove` + `git branch -D` for the merged branch | [checkout-as-isolation](../headless/protocol.md#checkout-as-isolation) — a no-op on the ephemeral runner, which is thrown away after the job |

Everything the table does not cite — the gate checks and QA-label gate, the conflict oracle and its `mergeable_state` cross-check, the strict-mode rebase handling, the Qodana fetch-and-fix, the un-draft, the merge-verification poll, the phase-label delete — is `/land`'s, verbatim.
