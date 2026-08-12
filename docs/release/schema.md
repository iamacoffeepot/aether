# Contributor workflow schema

Aether's contributor workflow is encoded in issue-body artifacts and observable
GitHub/repository facts. Labels classify issues; they do not carry progress,
approval, size/model routing, or landability. The executable Codex contract is
the shared [GitHub workflow contract](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/_shared/github-workflow.md)
plus the matching skill under `.agents/skills/`. Claude Code follows the same
artifact invariants through `CLAUDE.md` and `.claude/skills/`.

This page names the durable vocabulary. Read the executable source for exact API
mutations, trust validation, parsers, and pause boundaries.

## Issue taxonomy

Issue labels are searchable classification. The conventional-commit title and
exactly one `type:*` label identify the change type; `crate:*` labels identify
affected Cargo package scopes. Other labels may classify product or triage
concerns. None of them proves that scope is complete, selects an implementation
model, records approval, or authorizes a merge.

An open issue without a complete managed Plan is unscoped. An open issue with a
complete valid Plan is planned. A current trusted approval record makes that
specific Plan/base pair approved. A merged closing PR and closed issue are done.

## Managed issue sections

Scope owns these H2 sections in this order:

```text
## Problem statement
## Design notes
## Implementation plan
## Sub-issues
## Depends on
## Declared surface
## Dogfood brief
## Side findings
```

`Sub-issues`, `Depends on`, and `Side findings` are optional. The other five are
required for a planned issue. Duplicate headings are invalid.

The Implementation plan ends with exactly three non-empty lines:

```text
**Size:** <s|m|l>
**Implementation model:** <haiku|sonnet|opus>
**Routing reason:** <one concise reason>
```

Those lines are the routing source. Similar-looking labels are taxonomy drift,
not authority.

The canonical Plan digest includes the exact managed spans for Problem
statement, Design notes, Implementation plan, optional Sub-issues and Depends
on, Declared surface, and Dogfood brief. It excludes Side findings and unmanaged
prose. Use `.agents/skills/approve/scripts/plan_digest.py`; byte-level details
belong to that parser.

## Declared surface and dependencies

An implementable Declared surface is one fenced list containing safe concrete
repository paths or narrow directory prefixes with one final `/**`. It covers
every planned target and nothing broader. The approval policy resolves the most
restrictive `auto|judge|human` tier across everything each entry may permit.
The implementation and actual PR diff must stay inside the same boundary.

Dependencies live under `## Depends on` and must be complete before approval.
A pure umbrella has non-empty children, coordination-only work, and exactly:

```text
N/A — pure umbrella; no implementation PR
```

It can be approved for coordination but is never dispatched as implementation.

## Trusted approval record

Approval is one canonical hidden `aether-approval:v2` record in the issue body's
unmanaged prefix before the first managed H2. Its strict payload binds:

| Field | Meaning |
|---|---|
| `issue` | The positive issue number |
| `plan_sha256` | Digest of the current managed Plan |
| `size` / `model` | Exact body routing values |
| `policy_tier` | Tier resolved from approved paths and targets |
| `effective_tier` | Tier after applicable ADR routing |
| `authority` | `owner` or permitted unattended `policy-auto` decision |
| `base_sha` | Exact implementation base commit |

Trust comes from GitHub's effective issue-body editor provenance, not from the
payload's authority text. Owner authority requires the repository owner to be
the effective editor; policy-auto requires the editor and current policy to
permit it. A failed or ambiguous provenance read is unknown authority.

A current approval matches the freshly recomputed issue number, digest,
size/model, tiers, authority rules, and captured base. Managed approval-bearing
edits change the digest, and a different base requires another record. Older
records remain history. Approval is never a visible machine-JSON comment.

## Observable implementation states

The workflow derives progress from concrete artifacts:

| Observable state | Meaning |
|---|---|
| Open issue, incomplete managed artifacts | Scope is not complete |
| Complete managed artifacts, no current approval | Planned but unauthorized |
| Current trusted approval, no owned implementation artifact | Eligible for implementation |
| Owned issue worktree or branch | Implementation is in progress or paused |
| Open draft pull request | Reviewable implementation exists |
| Draft current head with pending/red required checks | Build/test proof is incomplete |
| Green current head without accepted review/thread/dogfood facts | QA evidence is incomplete |
| Green, contained current head with accepted review, clear threads, and required dogfood | Landable draft; explicit landing authority is still required |
| Named pull request merged and closing issue closed | Done |

Never infer one row from another. Each consumer re-reads the exact facts it
needs.

## Worktree and branch identity

Implementation uses one issue branch cut from the approval's exact base and one
surface-owned issue worktree:

```text
Codex:       <shared-root>/.agents/worktrees/issue-<N>
Claude Code: <shared-root>/.claude/worktrees/issue-<N>
```

The branch, worktree, issue, and optional PR must correlate unambiguously.
Existing artifacts are ownership evidence, never permission to delete them or
start a parallel implementation.

## Draft pull-request evidence

All gates describe one exact current head SHA.

### Approval and containment

The selected approval base must exist and be an ancestor of the head. The
current issue digest and route must still match its trusted record. Enumerate the
actual PR changed paths and require every one to match the Declared surface.

### Checks

Require every repository-required check for the current head to complete
successfully. Current branch protection requires `CI pass` and `Lint title`.
Local checks support this evidence but do not replace it.

### Direct review and native blockers

Direct review records a strict two-line `aether-direct-review:v1` commenting
review whose payload contains the exact head SHA, Plan digest, pull-request
number, and semantic `APPROVE` or `REQUEST_CHANGES` verdict. A trusted artifact:

- comes from the current PR's paginated reviews endpoint;
- is a `COMMENTED` review from an owner, member, or collaborator;
- binds both its REST commit id and payload to the current head;
- binds the current PR and freshly recomputed Plan digest.

The newest matching trusted artifact is the semantic verdict. Native decisions
remain separate: each reviewer's latest active `CHANGES_REQUESTED` blocks until
that reviewer approves or GitHub reports it dismissed. Every unresolved review
thread blocks independently. Neither a native approval nor a marker posted in a
different GitHub object substitutes for the semantic artifact.

### Dogfood

A specific `N/A` Dogfood brief is the exemption. Otherwise the durable rollup
must name the current head and scoped surface, report complete engine cleanup,
and contain no actionable result. Evidence from an older head is stale.

## Repair and conflict handling

Review and dogfood findings are verified, fixed inside the approved surface or
justified with evidence, committed and plain-pushed, replied to, and resolved
only after their disposition is visible. Every push creates a new head that must
repeat checks, semantic review, and required dogfood.

A needed path outside the Declared surface, broken Plan premise, or incompatible
design returns to the matching managed scope artifact. It is not license to
expand the diff.

Content-conflict resolution preserves both branch and current-main intent in
three-way context. Claude Code's `/resolve <PR>` merges current `main` into the
same draft branch, resolves only inside the approved surface, and drives the new
head through the full evidence loop. It does not rebase, force-push, create a
second PR, or land.

## Landing and cleanup

Landing is separately authorized. Immediately before mutation it independently
revalidates issue identity and digest, approval and ancestry, actual diff and
surface, current-head checks, semantic and native reviews, threads, dogfood,
branch ownership, and merge prediction.

An eligible landing clears draft state, performs an ordinary squash merge, and
continues only after GitHub confirms the named PR is merged. Done additionally
requires that PR to close its issue. A clean exact issue worktree and local
branch may then be removed; dirty, locked, or uncertain artifacts remain for an
explicit sweep.

## Hosted and packaging boundaries

The checked-in Actions tree owns hosted behavior. Current branch protection has
the two required checks named above and no required-pull-request-review rule.
Direct-drive scope, approval, review, dogfood, conflict, and landing skills are
not hosted jobs merely because repository scripts or prose describe them.

Landing a PR is separate from building `dist/`, producing a package depot,
tagging a version, or publishing a release. See
[Distribution and packaging](https://github.com/iamacoffeepot/aether/blob/main/docs/guide/building/distribution.md)
for those terms.
