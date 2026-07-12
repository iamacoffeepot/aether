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
| Building | A PR exists and its head is new or not CI-green; when the issue carries `## Declared surface`, an escaping diff also stays here | [Reconciler](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/reconciler.yml) |
| QA | CI is green but review or dogfood still owes a verdict | Reconciler |
| Findings | Actionable QA labels or unresolved review threads remain | `findings` changes facts; Reconciler recomputes |
| Held | CI and QA facts are clear; the draft is eligible to land | `land` after explicit authorization |
| Done | The closing PR is merged and the issue is closed | `land` reconciles stale phase state |

`phase:building`, `phase:qa`, `phase:findings`, and `phase:held` are
computed resting states. The Reconciler is their sole writer. Skills open a PR,
push a new head, reply to findings, resolve threads, or publish a verdict; they
do not assert what those facts mean by writing one of the computed labels.
Every relevant event causes the Reconciler to derive the target again, and a
new push returns the issue to Building until the new head is proven. When an
issue contains a fenced `## Declared surface` glob block, a diff that escapes it
is also pinned at Building until the diff is trimmed or the repository owner
widens/waives that boundary; green CI does not bypass the gate. An issue with no
declared-surface section is currently un-gated by this check.

`Approval gate` is nevertheless a required status on every PR. Where there is
nothing to enforce—no closing issue, an out-of-domain phase, or no declaration—
the reconciler posts an explicit passing no-op instead of leaving the required
context waiting forever.

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
| Implement approved work | `implement` | CI-green draft PR; phase then computed by automation |
| Resolve review or dogfood findings | `findings` | Facts cleared; Reconciler can compute Held |
| Regress work because an earlier phase is wrong | `bounce` | Bounced with one target |
| Land a reviewed draft | `land` | Merged PR and Done issue |
| Audit existing code or a non-PR change | `review` | Read-only findings rollup |
| Trial a public surface as a fresh consumer | `dogfood` | Evidence and consumer-friction rollup |
| Reclaim proven-stale local state | `sweep` | Only the explicitly confirmed cleanup |
| Capture repeatable session friction | `retrospect` | Confirmed Backlog issues, if any |
| Draft a load-bearing decision | `adr` | Proposed ADR draft in its own worktree |

Use the frontmatter description in the current `SKILL.md` to confirm a skill
fits before invoking it. A workflow name is not blanket authority for adjacent
steps. In particular, `implement` never lands and `review` is not an automatic
tail step after implementation: PR-bound Rust changes receive the repository's
hosted review after the first green CI head.

## Planned implementation

Planned work begins from a scoped GitHub issue. `scope` owns the structured
Problem statement, Design notes, Implementation plan, dependencies, declared
surface, Dogfood brief, and optional side findings. `approve` verifies those
artifacts and is the explicit Plan-to-Ready gate; it does not repair incomplete
scope.

For non-ADR work, `approve` resolves the declared paths against
`.github/approval-policy.yml` with most-restrictive-wins semantics over every
path the declaration permits. A crate-wide subtree therefore includes its
manifest tier even when the planned edit names only source today. `auto` work
may advance without a new owner decision. `judge` work receives an independent
verdict, but the judge is currently shadow-only, so the owner still confirms
it. `human` work always waits for the owner. An explicit `ADR flag:` or a
declared `docs/adr/**` edit takes the human route before policy lookup;
ordinary citations to existing ADRs do not.

The hosted tick resolves Plan surfaces with the same canonical matcher and,
when any issue resolves to exactly `auto`, dispatches one batched approval
sweep per wave that walks every such issue through the full per-issue gates —
approval is read-mostly, so the whole queue shares one runner. The headless
gate resolves each tier again before writing Ready. `judge`, `human`,
ADR-bearing, missing-surface, and unresolved-policy outcomes remain at Plan for
their reader — and the sweep surfaces them on one ticket: everything a sweep
does not advance (a human-tier wait, a gate failure, a blocked dependency)
becomes a row in a single open `agent:digest` issue, grouped so decisions,
defects, and waits read apart at a glance, with a no-reply breadcrumb comment
on each listed issue linking back. Every sweep run converges the same open
digest to the current board; the owner's single reply — on GitHub or straight
from the notification email — approves any listed non-ADR issue and can direct
mechanical label fixes in the same breath. The reply waives who approves,
never the scope gates, and ADR-bearing rows are visibility-only. The verdict
reply (or a "dismiss") closes the digest, and the next sweep with uncovered
candidates opens the next one.

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
the issue. The PR remains draft while CI and automated QA work.

The [CI workflow](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/ci.yml) provides the build proof.
On a same-repository green Rust-changing head, the
[Review workflow](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/review.yml) produces the automated
review verdict. When the closing issue contains a non-`N/A` Dogfood brief, the
[Dogfood workflow](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/dogfood.yml) trials the public
surface after Review is green. The Reconciler observes these results; it does
not rely on an agent remembering to advance a flag.

Fork heads do not receive repository secrets, so those automatic Review and
Dogfood paths intentionally skip them. A maintainer must review the fork and,
when appropriate, use the workflows' explicit manual-dispatch path from the
trusted repository context. Never work around that boundary by exposing a
secret to fork code.

## Findings and the landing gate

Findings are evidence to verify against the code, not commands to copy. The
`findings` workflow inventories every actionable item, fixes it within approved
scope or records an evidence-backed decline, replies, and resolves an anchored
thread only after its disposition is visible. A fix push creates a new head,
so CI and QA must prove that head again.

Held means land-eligible, not merged. A local Codex session still needs the
user's explicit `land <PR>` request, or confirmation of an itemized land sweep,
because clearing draft state makes reviewed code releasable and may merge it.
Hosted automation may invoke its own checked-in headless wrapper only under its
repository-configured governor and interaction protocol. Neither green CI nor
the Held label authorizes an ad-hoc self-merge.

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
