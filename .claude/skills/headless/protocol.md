# Headless execution protocol

The shared interaction-surface override for the headless wrapper skills (`/scope-headless`, `/implement-headless`, `/land-headless`). It is a plain doc, not a skill — the `headless/` directory carries no `SKILL.md`, so the loader never surfaces it as a slash command. A wrapper references it by relative path; nothing invokes it directly.

Every wrapper executes its original SKILL.md verbatim for process and judgment. This doc governs only the interaction surface — the points where the interactive original assumes a human at the keyboard. A headless agent runs one-shot on an ephemeral GitHub Actions runner: no keyboard to answer a parked question, no session that survives the job exit, no persistent worktree, no operator to re-run. The overrides below adapt those touchpoints without disturbing the process the original encodes.

## Precedence

For the interaction surface only — waiting on a human, printing to a human, confirmation prompts, worktree creation, session persistence, and commit identity — this protocol governs and overrides the original. For everything else — process order, judgment calls, phase-label mechanics, gates, and CI handling — the original governs unchanged.

Precedence is by enumeration, not by adjective. Each wrapper carries an **Overrides** table that lists the exact original anchors it supersedes, cited by heading. An instruction is overridden if and only if it appears in that table; anything not listed is the original's, verbatim. There is no "which instruction wins" ambiguity to resolve at runtime — the table is the whole answer.

## re-entrancy-first

A crashed or re-dispatched job must re-derive its state from observable facts rather than trusting anything it stored, because it stores nothing — liveness is computed, never persisted. Before any process step, every wrapper runs a re-entrancy guard that reads the current state from GitHub:

- Is the PR already merged? (`gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.merged'`)
- Does the branch already exist?
- Which `phase:*` label is present on the issue?
- Is there already an unanswered `agent:awaiting-answer` label with a parked-question comment on this issue?

An unanswered park means the job is waiting on the owner — end the turn without re-asking. Otherwise begin the original's process at the phase the observed state implies — the agent posts no start-of-work comment of its own; the workflow has already posted the claim comment (`agent-work.yml`'s start-of-work step), and the agent authors only the comments [comment-discipline](#comment-discipline) sanctions. A crash between steps leaves the phase label un-moved, so the next dispatch resumes from the same observable point without a stored cursor.

## comment-discipline

The headless agent authors a comment only where a base skill or this protocol explicitly instructs one — it never posts an announcement, status, progress, or narration comment of its own. The claim/start-of-work comment is the workflow's: `agent-work.yml` posts it natively, before the agent's process begins. The sanctioned comment surfaces are exactly those the skills name: the [ask-and-park](#ask-and-park) question comment, a self-bounce reason (scope §Comments), `/findings` review-thread replies, and any digest or terminal-state comment a base skill's own steps instruct — an [end-turn-not-wait](#end-turn-not-wait) terminal comment exists only where the base skill's terminal step calls for one; scope, whose §Comments forbids progress comments, carries its terminal state on the `phase:*` label and posts nothing.

An unrequested comment is an uncontrolled write to a public page from a token-bearing session. The model composes comment bodies blind — no fixed template, no review — and the `${GITHUB_RUN_ID:-local}` shell placeholder that shipped unexpanded into a freelanced comment on PR #3413 is the proof: the same blind composition could just as easily interpolate an env value or a token into a public page. Keeping the model tier cheap is fine; the guardrail is text, not effort.

## ask-and-park

Where the original would ask a human — a scope Define or Design bounce, a scope §Comments self-bounce, a land dirty-conflict or Qodana bail-out, or any value judgment only the owner can make — the headless agent does not stop and wait. It posts a plain-prose question comment in the fixed shape below, applies the `agent:awaiting-answer` label plus the `agent:park:<task>` label naming which task an owner reply re-dispatches, and exits 0. The exit is clean: a parked question is a normal terminal state for a headless run, not a failure.

The owner's reply re-dispatches the job — agent-chat reads the task from the `agent:park:<task>` label and the work ref from the surface the reply lands on, then dispatches agent-work (#3336 moved this routing payload off the free-text marker into structured GitHub state). Best-effort reasoning-context carry rides the #3313 warm session pool, bucketed by task+model; the reliable path is re-deriving the state from the board, exactly as the approve park already does. V1's only answerer is the owner — there is no `ask:*` routing vocabulary yet.

The question comment carries the human question and options only — no HTML marker. The machine state lives entirely in labels (`agent:awaiting-answer` classifies the park, `agent:park:<task>` routes the re-dispatch), so `status-sweep.sh` and agent-chat's route step read structured GitHub state rather than parsing the comment. The numbered **Options** list is for the owner; the leading `**Parked on #<N>**` header is what `status-sweep.sh` selects as the displayed question:

```markdown
**Parked on #<N> — need a decision.**

<question in plain language, the load-bearing "why" first>

Options:
1. <option A> — <consequence>
2. <option B> — <consequence>

Reply with an option number or free-form; your reply re-dispatches this job.
```

Post the comment over REST (`gh api -X POST repos/iamacoffeepot/aether/issues/<n>/comments -F body=@<file>`, body written to a file first so its backticks and `$` are not shell-expanded) and apply both labels over REST (the same `…/labels` endpoints the original's phase reconcile uses). The question carries its load-bearing "why" first, then the options with their consequences, so the owner can decide from the comment alone.

## end-turn-not-wait

The headless agent never sleep-polls or blocks on a human. The original's terminal human-waits — scope's "stops at Plan, awaiting `/approve`", implement's "print to user … tell me to land", land's "wait for one confirmation" — each become: post the terminal state as a comment and end the turn. The dispatch that resumes the flow comes from elsewhere — the owner's reply, or the reconciler or tick of a sibling rung — so there is nothing to wait on in-job.

This targets waits on a human, not waits on CI. An in-job wait the original owns that needs no human — the Refine-loop `scripts/wave-status.sh --wait <pr>` CI poll, `/land`'s strict-mode rebase re-predict, its Qodana-sweep wait — is **not** overridden here; the wait itself stays. The distinction is the party being waited on: a human wait is replaced by end-turn plus re-dispatch, a CI wait is kept — but a one-shot runner cannot realize a long synchronous foreground block, so it realizes the kept wait the headless way described next.

### the CI-wait contract

A one-shot headless runner has no foreground it can block indefinitely, and the `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` ceiling that was meant to hold a `claude -p` process open across a pending harness-tracked background task (#3086) does **not** reliably do so in the agent-work land/implement box. Run 29206645072 (`work-land-3241`) backgrounded `scripts/wave-status.sh --wait <pr>` exactly as the earlier contract prescribed — ended its turn with no `ScheduleWakeup stop` and no release sign-off — and the process still printed `result: success` and exited ten seconds later with the wait pending (#3245); `review.yml`'s "two live runs died" evidence is the same shape for the Workflow-tool task type. So a kept CI wait must **not** be realized by ending the turn on a pending background task — its correctness cannot depend on the ceiling.

Realize a kept CI wait instead as a **mechanical in-turn poll** — the pattern `review.yml` ships and proves in this exact `claude -p` harness. When the agent reaches an original's `scripts/wave-status.sh --wait <pr>` poll (or any other in-job wait the original owns), it does **not** background it and end the turn. It takes cheap status-check turns in a loop: each turn runs one bounded check — a `scripts/wave-status.sh <pr>` snapshot (`ci:success` / `ci:failure` / `ci:pending`), or a short bounded `sleep` followed by a re-check — inspects the result, and if CI has not settled takes another turn with another bounded check. The box never produces a final reply while the wait is unsettled; once a snapshot shows the `CI pass` aggregator settled (or a fast-failing deterministic check has already gone red), it reads that verdict and continues the original's process in the **same box** — goto its next step, land the merge, and so on.

While a CI wait is unsettled, releasing the process is **forbidden**, because any process-release move ends the run before CI settles and abandons the loop:

- No `ScheduleWakeup stop` — stopping the dynamic loop releases the process to exit instead of taking the next poll turn (the failure in run 29200198847 / `work-land-3214`: the wait was released, `ScheduleWakeup stop=true` was called, and the process exited ten seconds later with nothing left to resume the land).
- No terminal sign-off that concludes the session — no "no action needed until then", no "I'll continue once it completes". Such a message reads as a completed turn with nothing outstanding and lets the process exit. Take the next poll turn instead; say nothing that presents the run as finished until CI has actually settled.

This is distinct from [ask-and-park](#ask-and-park): a park exits 0 and waits on the owner's reply to re-dispatch, whereas a CI wait is polled to settlement within the same box — no owner action, no `agent:awaiting-answer` label, no S3 sync, and no dependence on the process surviving an ended turn.

## checkout-as-isolation

The ephemeral runner is single-purpose and thrown away after the job, so isolation is the job boundary itself, not a nested worktree. The runner's `$GITHUB_WORKSPACE` checkout is the workspace directly. The original's `git worktree add "$main_root/.claude/worktrees/issue-<N>"` and its matching `/land` worktree-sweep tail are no-ops in this environment — the agent works in the checkout as it stands and skips both the add and the sweep.

The checkout copy has its `.claude` interactive-session hooks stripped (the `jq 'del(.hooks)' .claude/settings.json` step the review and dogfood workflows already run) because the SessionStart worktree-rebind hook is actively harmful in CI — it would rebind the session to a worktree that does not exist here.

## commit-identity-assert

Before any commit or merge, assert the owner's public git identity — `iamacoffeepot` / `me@iamateapot.dev` — for the checkout (#3215): the fleet works in the owner's name, so its commits author as the owner. This narrows, not weakens, the standing privacy rule: the owner's real name and personal email never appear in any commit the agent produces — the public identity above is the only sanctioned author.
