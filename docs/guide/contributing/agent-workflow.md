# From an idea to a landed change

Aether's contribution workflow is a state machine over GitHub issues and pull
requests. The state is deliberately visible to humans and agents alike: issue
labels say what should happen next, issue-body sections hold the scoped work,
and pull-request checks and review threads are the facts that move the
post-implementation phases.

This page is the map. It explains which source answers which question, who owns
each transition, and which workflow to reach for. It does not reproduce the
mutation procedures in the skills. Codex must read the named skill and its
shared contracts before acting.

## Authority depends on the question

There is no useful universal rule that one document always wins. Use the source
that owns the question:

| Question | Authoritative source |
|---|---|
| What does the user want, and what consequential action is authorized? | The current user or repository-owner request |
| How does Codex perform a repository workflow? | [`AGENTS.md`](https://github.com/iamacoffeepot/aether/blob/main/AGENTS.md), the active Codex tool schema, and the matching [Codex skill](https://github.com/iamacoffeepot/aether/tree/main/.agents/skills) |
| How does Claude Code or a headless Claude job perform it? | [`CLAUDE.md`](https://github.com/iamacoffeepot/aether/blob/main/CLAUDE.md) and the checked-in `.claude/` workflow for that surface |
| What behavior is implemented? | Current code and tests |
| Why is a load-bearing design shaped this way? | The applicable Accepted ADR and its supersession chain |
| What arguments does a live tool accept? | The active tool schema, not a prose copy |
| What does this running engine contain? | Live introspection such as `describe_kinds` and `describe_component` |
| What phase is an issue or PR in now? | Current GitHub state: issue state and labels, PR head, checks, labels, and review threads |
| What does CI or hosted automation do? | The checked-in workflow plus current repository protection and check state |

The guide is the digested, navigable explanation. When it disagrees with a
current tool schema or implementation, report the drift and trust the owning
source for the immediate task.

## Agent surfaces are intentionally distinct

Codex uses [`.agents/skills/`](https://github.com/iamacoffeepot/aether/tree/main/.agents/skills) as its executable
workflow. Those skills are written for Codex's current tools and shared
contracts. They do not runtime-read or translate Claude instructions.

Claude Code and the headless GitHub Actions jobs continue to use
[`CLAUDE.md`](https://github.com/iamacoffeepot/aether/blob/main/CLAUDE.md), `.claude/skills/`, and
`.claude/workflows/`. Those files can be useful source material when a workflow
is intentionally adapted, but they are not Codex commands.

Architecture, public APIs, issue phases, and observable GitHub facts are shared.
Tool syntax, pause mechanics, worktree locations, and worker routing can differ
by surface. Never make a task name or prompt pretend that the active tool
selected a model or role it cannot actually select.

## The canonical lifecycle

The complete phase vocabulary is documented in the
[release phase schema](https://github.com/iamacoffeepot/aether/blob/main/docs/release/schema.md):

```text
Backlog (open, no phase label)
→ phase:define
→ phase:design
→ phase:plan
→ phase:ready
→ phase:building
→ phase:qa
→ phase:findings
→ phase:held
→ Done (closed, no phase label)
```

Backlog and Done are both represented by label absence, so issue state matters:
an open issue with no `phase:*` label is Backlog; a closed issue is Done even
if a stale phase label still needs cleanup. An open issue in an active phase
has exactly one `phase:*` label. Multiple phase labels are invalid state, not a
choice for an agent to resolve by guessing.

| Phase | Durable meaning | Owner of the next transition |
|---|---|---|
| Backlog | An open idea not yet scoped | `scope` begins Define |
| Define | The problem and success criteria are being established | `scope` |
| Design | The approach, alternatives, affected surfaces, and ADR boundary are being established | `scope` |
| Plan | An executable plan, declared surface, and routing labels exist | `approve`, the policy-routed Plan-to-Ready gate |
| Ready | Approved and eligible for implementation | `implement` |
| Building | Contracted state: a PR exists and its head is new or not CI-green; a declared-surface escape also belongs here | Intended reconciler (hosted workflow unavailable) |
| QA | Contracted state: CI is green but review or dogfood still owes a verdict | Intended reconciler (hosted workflow unavailable) |
| Findings | Contracted state: actionable QA labels or unresolved review threads remain | `findings` changes facts; intended reconciler recomputes |
| Held | CI and QA facts are clear; the draft is eligible to land | `land` after explicit authorization |
| Done | The closing PR is merged and the issue is closed | `land` reconciles stale phase state |

`phase:building`, `phase:qa`, `phase:findings`, and `phase:held` are computed
resting states in the Codex lifecycle contract. The contract reserves their
writes for a reconciler: skills change observable facts rather than asserting a
computed phase. It also treats a fresh head or a declared-surface escape as
Building until the relevant facts are clear.

That is intended lifecycle semantics, not a claim about current hosted
automation. The checked-in Actions directory has no `reconciler.yml`,
`review.yml`, `dogfood.yml`, or `quality-eval.yml`, so GitHub cannot currently
perform those post-Ready transitions. There is likewise no hosted `Approval
gate` check. Branch protection requires only `CI pass` and `Lint title`, with no
required-pull-request-review rule configured.

`phase:executing` and `phase:refine` are retired migration inputs. Current
skills never write them.

## Choosing the workflow

| Intent | Codex workflow | Terminal state |
|---|---|---|
| Explore a felt absence before committing to work | `wish` | An idea tree, not a GitHub issue |
| Capture a rough, single idea | `sketch` | Backlog issue |
| Turn a Backlog or bounced issue into grounded scope | `scope` | Plan |
| File selected unrelated scope findings | `scope-spinoff` | Child Backlog issues |
| Approve completed scope | `approve` | Ready |
| Implement approved work | `implement` | Draft PR; CI is hosted, while contracted post-Ready automation is currently unavailable |
| Resolve review or dogfood findings | `findings` | Facts cleared for the intended reconciler to evaluate |
| Regress work because an earlier phase is wrong | `bounce` | Bounced with one target |
| Land a reviewed draft | `land` | Merged PR and Done issue |
| Audit existing code or a non-PR change | `review` | Read-only findings rollup |
| Trial a public surface as a fresh consumer | `dogfood` | Evidence and consumer-friction rollup |
| Reclaim proven-stale local state | `sweep` | Only the explicitly confirmed cleanup |
| Capture repeatable session friction | `retrospect` | Confirmed Backlog issues, if any |
| Draft a load-bearing decision | `adr` | Proposed ADR draft in its own worktree |

Use the frontmatter description in the current `SKILL.md` to confirm a skill
fits before invoking it. A workflow name is not blanket authority for adjacent
steps. In particular, `implement` never lands. The `$review` skill is a local
backfill audit, not an automatic tail step after implementation, and an absent
hosted Review workflow must not be inferred from the skill contract.

## Planned implementation

Planned work begins from a scoped GitHub issue. `scope` owns the structured
Problem statement, Design notes, Implementation plan, dependencies, declared
surface, Dogfood brief, and optional side findings. `approve` verifies those
artifacts and is the explicit Plan-to-Ready gate; it does not repair incomplete
scope.

For non-ADR work, `approve` resolves the declared paths against
`approval-policy.yml` with most-restrictive-wins semantics over every
path the declaration permits. A crate-wide subtree therefore includes its
manifest tier even when the planned edit names only source today. `auto` work
may advance without a new owner decision. The `$approve` `judge` tier is
currently shadow-only, so the owner still confirms it. `human` work always
waits for the owner. An explicit `ADR flag:` or a
declared `docs/adr/**` edit makes the issue ADR-bearing, and the ADR hard gate
is maturity-aware (ADR-0146 §6): a **new** ADR or an amendment to an
**established** (non-`Proposed`) one takes the human route before policy lookup,
while a change whose every touched ADR is still `Status: Proposed` defers to the
ordinary policy lookup (the `docs/adr/**` tier is `judge`). Ordinary citations
to existing ADRs make an issue neither ADR-bearing nor human-routed.

`$approve` is the checked-in Plan-to-Ready procedure. It re-reads the issue,
resolves the policy tier, checks freshness, dependencies, scope, routing, and
ADR requirements, and pauses for owner input when the contract requires it.
The skill is the single source for its exact gates, sweep behavior, and label
mutations. No checked-in hosted tick dispatches approvals automatically.

The owner can pre-authorize one non-ADR issue at any earlier phase with
`approval:pre-approved`. The approval gate verifies the actor of the latest
matching label event, then treats a genuinely owner-applied label as an
effective `auto` tier. It still runs every scope, freshness, dependency, and
model gate; the label waives who must approve, not whether the Plan is valid.
It never overrides the ADR hard gate, and an agent-applied copy of the label
grants no authority.

A pure umbrella has no implementation path or PR. It uses the exact
`N/A — pure umbrella; no implementation PR` declaration, stays human-routed,
and remains ineligible for `implement`.

`implement` accepts Ready work, creates or adopts the one issue worktree
described on [Worktrees and safety](worktrees-and-safety.md), follows the
approved plan, runs the required local checks, and opens a draft PR that closes
the issue. The PR remains draft while the required lifecycle facts are gathered.

The checked-in [CI workflow](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/ci.yml)
provides the build proof. The `$implement` contract also describes integrated
Review, Dogfood, and reconciliation after a green head, but the corresponding
hosted workflow files are absent from current `main`. Those steps are therefore
unavailable; related repository scripts do not create workflow triggers by
themselves. `$review` and `$dogfood` remain explicit local skills for the cases
named in their own contracts, not substitutes for silently claiming that
PR-bound hosted QA ran.

## Findings and the landing gate

Findings are evidence to verify against the code, not commands to copy. The
`findings` workflow inventories every actionable item, fixes it within approved
scope or records an evidence-backed decline, replies, and resolves an anchored
thread only after its disposition is visible. A fix push creates a new head,
so CI and QA must prove that head again.

Held means land-eligible, not merged. A local Codex session still needs the
user's explicit `land <PR>` request, or confirmation of an itemized land sweep,
because clearing draft state makes reviewed code releasable and may merge it.
Neither green CI nor the Held label authorizes an ad-hoc self-merge.

Landing verifies the current head and required checks again, clears draft state,
confirms the merge through GitHub, then reconciles the closing issue and only
removes a clean, proven-owned worktree. See [Local checks and CI](../local-verification.md)
for the verification split.

## Bounce, stall, and resume

`phase:bounced` means the product work needs an earlier phase redone. It carries
exactly one of `bounce-to:define`, `bounce-to:design`, or `bounce-to:plan` and a
human-readable reason. `scope` consumes that target and reruns it plus the
downstream scope phases. A design discovery bounces to Design; a stale or
incomplete implementation plan bounces to Plan; an unframable problem returns
to Define.

`phase:stalled` is different. It records an environment, authentication,
network, runner, or service failure rather than a defect in the issue's scope.
It preserves the branch, draft PR, and worktree for an explicit resume after
the external condition clears. Do not use a bounce merely because work is
difficult, slow, or temporarily unavailable.

## Human contributors

Humans and agents read the same issue labels, scoped sections, draft state,
checks, and review threads. A human does not need to imitate agent tool syntax,
but should preserve the same invariants: one focused concept, a linked issue for
planned work, Conventional Commit titles, no direct push to `main`, and no
merge while required facts or findings remain open.

The executable Codex contracts are the
[Codex harness](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/_shared/codex-harness.md) and
[GitHub workflow contract](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/_shared/github-workflow.md).
Return to this page to understand the journey; read the applicable contract
before changing repository or GitHub state.
