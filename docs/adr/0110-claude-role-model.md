# ADR-0110: Claude Role Model

- **Status:** Accepted
- **Date:** 2026-06-13

## Context

Multiple Claude sessions run against this repository at once — ideating, scoping, implementing, landing — and they share one clone and one GitHub account. Two failures recur. Sessions collide in the working tree (one switches branches or edits files under another's feet), and several sessions hammering the GitHub GraphQL API at once exhaust the per-user pool, which stalls work mid-flight (observed at roughly three concurrent sessions).

Authority also drifts. A session opened to ideate ends up merging to main; one opened to implement re-scopes an issue. Nothing scopes what a session is *for*, so the blast radius of any single session is the whole pipeline.

The pieces a role model can build on already exist: a skill vocabulary (`wish`, `wish-deep`, `sketch`, `scope`, `scope-spinoff`, `approve`, `implement`, `bounce`, `sweep`) and an issue's whole lifecycle on labels — `phase:*` carries the Phase vocabulary (Backlog → Define → Design → Plan → Ready → … → Done, with Backlog and Done as label-absence), alongside `type:*`, `size:*`, `model:*`. Phase rides labels rather than a GitHub Project board because ProjectsV2 is GraphQL-only, and the per-user GraphQL pool is the same contended resource this model bounds; carrying phase as a REST label keeps every phase write off that pool. What is missing is a binding from a session to a **role** that fixes which skills it runs, where it runs them, and what it is allowed to touch.

## Decision

### Roles

A session declares one of four roles at start. Each role names a skill set, a loop, and the go/no-go gate where the loop pauses for the user.

- **dreamer** — turns a felt absence into scoped issues. Skills: `wish`, `wish-deep`, `sketch`, `scope`, `scope-spinoff`, `sweep fat`. Loop: wish a theme → drill → weigh each candidate → file the skinny ones / decompose the fat ones → scope to Plan. Gate: per decomposition plan, and at "scoped to Plan, awaiting approve."
- **scoper** — a narrow dreamer for the occasions a session does scope-only work. Skills: `scope`, `bounce`, `scope-spinoff`. Loop: take a Backlog issue → walk Define → Design → Plan → stop. Gate: at Plan (awaiting approve).
- **orchestrator** — turns scoped issues into merged PRs end-to-end, landing included. Skills: `approve`, `implement`, `bounce`, `land`, `sweep`. **Shardable** (see below). Loop: approve a batch → dispatch `implement` (background agents) → run CI to green → hold draft → *(user go)* land → delete the `phase:*` label (Done) → sweep. Gate: the un-draft/land review gate.
- **everything** — no directive, every skill, the ad-hoc escape hatch. Worktree-bound only.

### Weight and decomposition

Every sketched issue carries a weight, assigned during the sketch process:

- **skinny** — Size is S, M, or L: fits one focused PR, ready to flow scope → approve → implement.
- **fat** — Size is XL: an arc that would span multiple PRs and cannot be scoped or implemented until broken down.

Weight reuses the existing `size:*` label set, extended with a fourth value, **`size:xl`**, meaning fat. The weigh step stamps it explicitly, so "needs breakdown" is a deliberate mark rather than an inference from an absent label — an absent `size:*` means un-triaged, not fat. Fat issues are decomposed recursively, mirroring how `wish --deep` drills until every branch is producible, here terminating when every leaf is skinny. A decomposed fat parent is closed and replaced by its skinny children, which link back to it; only skinny issues stay live. `sweep fat` enumerates fat issues and drives each through decomposition. This turns the "one focused issue per shippable PR, no phased mega-issues" rule into a machine-visible state rather than a judgment call at review time.

### Session binding

Each session is bound to its own worktree under `.claude/worktrees/<session-id>`, created at start and never swapped out. The role marker is a gitignored, session-keyed file (`.claude/roles/<session-id>`) in the main clone, written once the role is known and read by session id from each consumer's input — the binding hook, the guardrail hook, and the status line. A `SessionStart` hook creates the worktree, reads the marker, and injects that role's directive — the skill set, the loop, the gate. With no marker, the hook injects an instruction to ask the user the role and write the marker. The directive ends with the `loop` invocation over the role's skill sequence, pausing at the role's gate for the user's go/no-go. The role is durable and session-bound, so it survives a restart. A per-session status line renders the role as a colored label, read from the same marker, so the kind of session is legible at a glance.

The hook cannot change the running session's cwd itself (a hook's cwd is fixed at launch), so ahead of the directive it injects an instruction making the session's first action a call to the `EnterWorktree` tool with the session-worktree path — a model tool, unlike a hook, that does move the session's cwd into the worktree. Edits and commands then land in the worktree by default, and the `PreToolUse`/`PostToolUse` enforcement below is the backstop for anything that slips past rather than the primary mechanism.

The session worktree is **git-locked** at bind time and unlocked on session end, so a removal can never pull it out from under a live session. The lock is the load-bearing guard because it closes the path for every remover at once rather than per caller: the `SessionStart` hook runs `git worktree lock --reason "active claude session <id>"` right after creating the worktree, and `git worktree remove` then refuses it — whether the removal comes from a `/sweep` run, an ad-hoc cleanup, or another concurrent session's sweep. Without it, a clean-but-undiverged worktree (no commits ahead of `main`, no dirty files, no PR) is indistinguishable from abandoned cruft and gets reclaimed. A `SessionEnd` hook releases the lock so the dead worktree becomes reclaimable on a clean exit; a crash that skips `SessionEnd` leaves it locked, which `/sweep` resolves by probing for a live process whose cwd is the worktree before it unlocks and removes a stale one.

### Hook-enforced guardrails

> **Revised by [ADR-0111](0111-soft-ask-boundaries.md):** the hard `exit 2` block described below is replaced by an ask-to-confirm gate (`permissionDecision: "ask"`). The boundaries and their scope still hold; only the enforcement action (deny → ask) changes.

A `PreToolUse` hook reads the role marker and worktree path and blocks, with a reason:

- **Worktree boundary (all roles)** — a change that dirties a tracked file in the **main** worktree. Reads of any file, and writes under `/tmp` or the session's own worktree (`.claude/worktrees/<session-id>`), are allowed; only a change that would leave the main checkout dirty is held back. The edit tools (Edit/Write/MultiEdit/NotebookEdit) declare their target up front, so an edit landing in the main worktree is blocked before it runs (`PreToolUse`); a Bash command's effect is open-ended, so a dirtied main checkout is caught and reverted after it runs (a `PostToolUse` `git status` tripwire). Agents a session spawns run outside this guardrail (no role marker of their own, so the hook fails open) and work in their own worktrees, so a session stays bound to its own worktree and reaches its agents by dispatch rather than by editing theirs.
- **Role boundary** — dreamer and scoper are blocked from `approve`, `implement`, merge, and code push; orchestrator is blocked from `wish`, `sketch`, and issue creation (a design gap bounces back rather than being scoped in place); everything carries no role boundary.

Enforcement is the payoff. Advisory boundaries are the drifting status quo; a hard block is what removes the shared-clone collisions and bounds each session's blast radius.

### Orchestrator sharding and coordination

Orchestrator is the one shardable role: several orchestrator sessions run concurrently, each in its own session-worktree, each owning a disjoint slice of issues end-to-end including its own merges. This keeps any single orchestrator's context bounded by its slice rather than by the whole release, and keeps merge authority with the role doing the work.

Sharding forces coordination, since all shards share one GitHub account:

- **Partitioning** — the user hands each shard its batch as its go (user-directed partitioning fits the go/no-go loop), with a visible claim via GitHub issue assignee. Assignee writes go through REST, so a claim costs no GraphQL.
- **GraphQL pool** — the per-user GraphQL limit is the practical cap on how many shards run hot at once. With phase carried on REST labels the pipeline issues almost no GraphQL — the lone GraphQL-only op is the PR un-draft (`markPullRequestReadyForReview`, once per PR in `land`). The existing mitigations still apply: stagger dispatch and prefer REST forms (merges, labels, assignees, issue creation) over GraphQL. There is no cross-session lock on the pool, so the safe concurrent count is operational, not enforced.

## Consequences

- The two standing failures go away by construction: worktree isolation plus the `PreToolUse` boundary removes shared-clone collisions, and the role boundary removes runaway authority (a dreamer cannot merge; an orchestrator cannot re-scope).
- Orchestrator context stays bounded by its shard, and sharding scales throughput up to the GraphQL ceiling.
- New build surface this creates: the `SessionStart` and `PreToolUse` hooks; four role-directive files; a status line script that color-codes the role label from the session-keyed marker; a `land` skill that formalizes the currently ad-hoc landing mechanics (un-draft → auto-merge → delete the `phase:*` label for Done → sweep); and a `sweep fat` target. Editing `.claude/hooks/*` and `settings.json` is guardrail self-modification, so that work lands only under explicit authorization.
- Weight reuses the `size:*` label set, extended with one new value (`size:xl` = fat). An explicit `size:xl` keeps fat distinct from an issue that is merely un-triaged (no `size:*` label).
- `wish --deep` returning weighted issue sketches is enabled by this model — its terminal nodes become weighted sketches the dreamer files or decomposes — but it is a separate `wish-deep` change, filed on its own, not part of this ADR.
- The GraphQL pool stays a global shared resource with no cross-session lock, so sharding raises contention before the mitigations absorb it; the safe orchestrator count is a thing to watch operationally.

## Alternatives considered

- **Reviewer as a distinct role** — rejected; review stays inside orchestrator's implement → green loop, where `code-review` and `verify` run before landing.
- **Merger as a distinct role** — rejected in favor of sharding orchestrator. Merge authority stays with the role doing the work, and blast radius is bounded by the shard plus the landing gate, so isolating the merge stage buys little and adds a handoff seam.
- **Advisory (non-enforced) guardrails** — rejected; advisory boundaries are the status quo that drifts. Hook enforcement is the whole point.
- **A dedicated Weight label (skinny/fat)** — rejected; weight is the same axis as Size, so it lives on the `size:*` label set as a new `size:xl` rather than a parallel label.
- **Inferring fat from an absent `size:*` label (unset = fat)** — rejected; it dodges adding a value but overloads an un-triaged issue as "needs breakdown." An explicit `size:xl` is the deliberate mark.
- **Keeping fat parents as tracking epics** — rejected; that reintroduces the multi-PR mega-issue the team retired. Close-and-replace keeps only skinny issues live.
- **Session-chosen role with no worktree binding** — rejected; it loses the worktree-to-session isolation that prevents shared-clone collisions.
- **Splitting the merge stage out to shrink orchestrator context** — rejected on diagnosis; orchestrator context grows from the implement phase (diffs, CI loops), which `implement` already offloads to background agents. Sharding addresses the context size; moving the merge tail would not.
- **Per-role color theme** — rejected as unavailable; the Claude Code theme is read-only and `SessionStart` hooks cannot set it, so role visibility goes through a colored status line label instead.
