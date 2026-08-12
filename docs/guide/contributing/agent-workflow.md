# From an idea to a landed change

Aether's contributor workflow is direct-drive. Issue-body artifacts define the
approved work, an owned worktree and branch hold implementation, and a draft
pull request accumulates current-head evidence. The checked-in skills perform
each operation through the active agent surface; GitHub Actions supplies build
and test checks, not lifecycle orchestration.

This page explains the journey and its invariants. It does not copy mutation
procedures from the skills. Read the matching skill and shared contract before
changing repository or GitHub state.

## Authority depends on the question

Use the source that owns the question:

| Question | Authoritative source |
|---|---|
| What does the user want, and what consequential action is authorized? | The current user or repository-owner request |
| How does Codex perform a repository workflow? | [`AGENTS.md`](https://github.com/iamacoffeepot/aether/blob/main/AGENTS.md), the active Codex tool schema, and the matching [Codex skill](https://github.com/iamacoffeepot/aether/tree/main/.agents/skills) |
| How does Claude Code perform it? | [`CLAUDE.md`](https://github.com/iamacoffeepot/aether/blob/main/CLAUDE.md), the active Claude tools, and the matching `.claude/skills/` contract |
| What behavior is implemented? | Current code and tests |
| Why is a load-bearing design shaped this way? | The applicable Accepted ADR and its supersession chain |
| What arguments does a live tool accept? | The active tool schema, not a prose copy |
| What does this running engine contain? | Live introspection such as `describe_kinds` and `describe_component` |
| What work is approved? | The current managed issue sections plus a trusted matching hidden approval record |
| Is a draft ready to land? | Its exact head, approval ancestry, actual diff, checks, reviews, threads, dogfood evidence, and merge state |
| What does hosted automation do? | Checked-in workflow YAML plus current repository protection and check state |

The guide is the digested, navigable explanation. When it disagrees with a
current tool schema or implementation, report the drift and trust the owning
source for the immediate task.

## Agent surfaces are intentionally distinct

Codex uses [`.agents/skills/`](https://github.com/iamacoffeepot/aether/tree/main/.agents/skills)
and their shared contracts. Those files are written for Codex's current tools;
they do not runtime-translate Claude instructions.

Claude Code uses `CLAUDE.md` and `.claude/skills/`. The two surfaces share the
same issue-body artifact, approval, containment, review, and landing invariants,
while command syntax, pause mechanics, worker routing, and worktree roots may
differ. A prompt or task name cannot select a model or role that the active tool
did not actually select.

## The durable journey

```text
idea or rough issue
  → managed Plan sections + declared surface + body routing lines
  → trusted hidden approval bound to Plan digest + exact base
  → owned issue worktree + branch
  → draft pull request closing the issue
  → current-head checks + review + threads + required dogfood
  → explicitly authorized landing
  → merged pull request + closed issue + safe local cleanup
```

These artifacts are independent facts. A branch does not prove approval. Green
CI does not prove that the current diff was inspected. Direct inspection does
not clear a native change request or unresolved thread. A closed issue does not
prove that a named pull request merged.

Issue labels are taxonomy only. They can identify the conventional-commit type,
affected Cargo scopes, or other searchable classifications, but they do not
carry workflow progress, approval, or model routing. Read the body and concrete
implementation artifacts instead.

## Choosing the workflow

| Intent | Codex workflow | Durable result |
|---|---|---|
| Explore a felt absence | `wish` | An idea tree, not automatically an issue |
| Capture a rough single idea | `sketch` | An open unscoped issue |
| Ground the problem, design, Plan, surface, and route | `scope` | Complete managed issue-body artifacts |
| File selected unrelated scope observations | `scope-spinoff` | Linked unscoped issues |
| Authorize a complete Plan | `approve` | A trusted hidden digest/base-bound record |
| Implement approved work | `implement` | A reviewed, green draft PR with required dogfood clear |
| Audit existing code or a non-PR change | `review` | A read-only findings rollup |
| Trial a public surface as a fresh consumer | `dogfood` | Durable evidence and a consumer-friction rollup |
| Land an accepted draft | `land` | Merged PR, closed issue, and safe cleanup |
| Reclaim proven-stale local state | `sweep` | Only the explicitly confirmed cleanup |
| Capture repeatable session friction | `retrospect` | Confirmed unscoped issues, if any |
| Draft a load-bearing decision | `adr` | Proposed ADR draft in its own worktree |

Claude Code exposes the corresponding slash-prefixed skills and additionally
uses `/resolve <PR>` for a content-conflicted draft. Always read the current
skill frontmatter before invoking it. One workflow name does not authorize an
adjacent consequential action; implementation never implies landing.

## Scope and routing live in the issue body

`scope` owns the managed sections for the problem statement, design notes,
implementation plan, optional sub-issues and dependencies, declared surface,
dogfood brief, and optional side findings. The Plan ends with exact `Size`,
`Implementation model`, and `Routing reason` lines. Those body lines—not labels—
select the implementation route.

The declared surface is a strict list of concrete paths and narrow directory
prefixes. It bounds approval-policy resolution and the eventual PR diff. A
necessary edit outside it is evidence that the Plan must change, not permission
to widen implementation. A pure umbrella declares that it has no implementation
PR and closes only after its children and coordination obligations are complete.

The canonical Plan digest covers the approval-bearing managed sections and
their exact bytes. Side findings and unmanaged prose are outside that identity.
Use the checked-in parser; do not recreate its normalization in prose or a new
script.

## Approval binds intent to code history

`approve` re-reads the issue, verifies its structure, dependencies, ADR needs,
targets, declared-surface containment, routing, and policy against a freshly
captured `origin/main`. It appends a canonical hidden `aether-approval:v2`
record to the issue body's unmanaged prefix. The record binds:

- the issue number;
- Plan digest;
- size and implementation model;
- policy and effective approval tiers;
- permitted authority; and
- the exact base commit.

Trust comes from the effective issue-body editor reported by GitHub, not from an
authority string inside the payload. An edit to approval-bearing managed bytes
changes the digest; a different base needs another approval. Older records stay
as history but do not authorize the current Plan.

The policy tiers are `auto`, `judge`, and `human`. Their authority rules do not
weaken structure, freshness, dependency, ADR, or containment gates. Approval
does not edit scope or start implementation.

## Implementation creates one reviewable artifact

Fresh implementation requires a current trusted approval whose base is current
`origin/main`. The active surface creates one issue branch and one issue
worktree from that exact commit:

- Codex: `.agents/worktrees/issue-<N>`;
- Claude Code: `.claude/worktrees/issue-<N>`.

The implementation follows the Plan literally, runs focused verification plus
`cargo fmt -- --check` and `cargo clippy --all-targets -- -D warnings`, reviews
the complete diff, checks every changed path against the declared surface, then
plain-pushes and opens a draft PR that closes the issue. Existing artifacts are
possible live ownership claims and require a verified resume, never opportunistic
deletion or recreation.

GitHub Actions proves the build/test tree. Current branch protection requires
`CI pass` and `Lint title`; it does not configure required pull-request reviews.
The checked-in [workflow README](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/README.md)
owns the exact hosted inventory.

## Review, findings, and dogfood are head-bound

After CI is green, the implementer directly inspects the complete current-head
diff against the Plan and records the result in the ordinary human-readable
implementation handoff. The implementer is the reviewer for this loop; it does
not post JSON or HTML machine markers in PR reviews or comments.

Review acceptance has three separate gates:

1. direct inspection of the exact current head found no unresolved defect;
2. no reviewer's latest active native decision is changes requested; and
3. every review thread is resolved.

Actionable findings enter the implementation's integrated repair loop. Verify
each item, fix it within the approved surface or give a concrete justification,
push an ordinary commit, rerun local checks and CI, reply to its anchored thread,
and resolve the thread only after the disposition is visible. The changed head
then needs fresh direct inspection. A root-level or out-of-scope problem returns
to the appropriate managed scope artifact instead of being silently waived.

The issue's Dogfood brief says either why no consumer trial applies or defines
the exact consumer task. A required run must identify the current head and
surface, preserve its evidence, clean every run-owned engine, and have no
actionable result. A corrective push makes older review and dogfood evidence
stale.

## Conflicts preserve both intents

Landing predicts the merge against current `main`. A content conflict is not
permission to choose a resolution inside the landing step. Claude Code hands the
draft to `/resolve <PR>`, which merges current `main` into the same branch,
resolves every hunk in three-way context inside the approved surface, and drives
the resulting head through checks, review, repair, and dogfood again. It does not
rebase, force-push, open a second PR, or merge.

Other surfaces stop with the exact conflict evidence and use their checked-in
contract or explicit owner direction for the equivalent resolution. A genuinely
incompatible product intent returns to scope rather than manufacturing a merge.

## Landing is a separate authorization boundary

A draft can be landable without being authorized to merge. `land` independently
revalidates the current issue digest and approval, base ancestry, actual diff and
declared surface, required checks, semantic and native review state, threads,
dogfood, branch ownership, and predicted merge result.

Only then does the explicitly authorized landing clear draft state and perform
an ordinary squash merge. Cleanup starts only after GitHub confirms that named
PR merged and its closing issue is closed. The exact issue worktree and local
branch are removed only when clean; uncertain or dirty artifacts remain for an
explicit sweep.

## Human contributors

Humans do not need to imitate agent tool syntax, but they should preserve the
same invariants: one focused concept, a linked and scoped issue for planned work,
Conventional Commit titles, isolated implementation, no direct push to `main`,
and no merge while current-head facts or findings remain open.

The executable Codex contracts are the
[Codex harness](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/_shared/codex-harness.md)
and [GitHub workflow contract](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/_shared/github-workflow.md).
Return here for the journey; read the applicable skill before mutating state.
