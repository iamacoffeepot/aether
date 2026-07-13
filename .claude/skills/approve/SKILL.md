---
name: approve
description: Plan → Ready gate. Validates that an issue's scope artifacts are complete and any drafted ADR has merged, then advances Phase to Ready. Does NOT dispatch implementation — that's /implement's job. Idempotent on re-run. `--sweep` discovers and batch-approves every Plan-complete issue behind one confirmation.
---

# /approve — Plan → Ready gate

The primary human review point of the release flow. The user invokes `/approve <issue>` after reading the scope artifacts that `/scope` produced. The skill validates the gates and flips the issue to Ready; from there `/implement` (or the Phase C orchestrator) picks it up. `phase:ready` is `/approve`'s only ladder write — the issue stays at Ready through implementation, and the reconciler moves it to the computed `phase:building` when the PR opens.

## Sweep approve

`/approve --sweep` is the batched discovery entry point: instead of taking issue numbers, it enumerates every Plan-complete issue, validates each against the same gates the single-issue path runs, and waits for one confirmation before flipping any to Ready. It mirrors `/implement --sweep` (the dispatch-side discovery mode) so a reviewer clears a whole scoped batch in one pass instead of typing each number.

1. **Enumerate over REST, in one call.** `phase:plan` is set only by `/scope` when it lands an issue at Plan, so the label alone is the eligibility signal — one REST query, off the contended GraphQL pool:

   ```bash
   gh api 'repos/iamacoffeepot/aether/issues?labels=phase:plan&state=open' --jq '.[].number'
   ```

   This is the REST issues endpoint (per `/scope` §REST-vs-GraphQL routing), not `gh issue list`, which is GraphQL-backed and drains the contended pool.

2. **Gate-check each candidate.** Run the full [gate checks](#gate-checks) per issue — `Phase == Plan`, the three §-sections present and non-empty, every referenced ADR PR merged, exactly one `model:*` label, not blocked by a `blocked`/`wontfix`/`duplicate` label, freshness gate (targeted paths exist on `origin/main`; churn re-grounds against the plan's anchors, parking only a broken anchor), dependency gate (every `#N` in `## Depends on` is a closed issue). Drop any issue that fails and record the reason; the sweep never silently skips — every dropped issue is listed in the plan with its drop reason. `--skip-adr` is **not** honored in sweep mode: a batch is the wrong place for a per-issue emergency override, so an unmerged-ADR issue is dropped and listed, to be approved singly with `/approve <n> --skip-adr` if the override is intended.

3. **Print the approve plan and wait for confirmation.** A batch label write is cheap to do but annoying to unwind, so one confirmation prompt covers the set. Print the issues that will be approved (with their `size:*` / `model:*` for context), any umbrella issues flagged distinctly (an umbrella with `## Sub-issues` is approvable — approving means "the plan is approved, children split correctly" — but it is not itself `/implement`-able; see [Multi-PR umbrella issues](#multi-pr-umbrella-issues)), and the dropped-with-reason list, then stop and wait:

   ```
   Sweep: 6 Plan issues, 2 dropped, 4 to approve.

   Approve → Ready:
     #1756  back every actor inbox with the settling-inbox primitive   size:m  model:sonnet
     #1757  single-ownership dispatched envelope via take_inbound       size:m  model:opus
     #1758  migrate capture replies to the retained inbound guard       size:l  model:opus
     #1754  close mail lineage … (umbrella — plan approved, not dispatched)

   Dropped:
     #1719  Phase=Design, not Plan
     #1740  ADR PR #1738 not merged (approve singly with /approve 1740 --skip-adr to override)
     #1762  Targets removed on main: crates/aether-capabilities/src/audio/mod.rs

   Confirm approve? (no label write happens until your go-ahead)
   ```

4. **On confirmation, approve the batch.** Apply [Actions on pass](#actions-on-pass) over the passing set — reconcile each passing issue's label to `phase:ready` (a REST `PUT …/labels` per issue, see [Phase label reconcile](#phase-label-reconcile)). The sweep never auto-confirms. `--sweep` dispatches no background agents — the parent applies the `phase:ready` label PUTs inline — so there is no concurrency cap to set.

`--sweep` takes no issue argument — it discovers them. It does not combine with `--note` or `--skip-adr`, both single-issue concerns.

## Invocation

```
/approve <issue>                    standard (single issue)
/approve <issue> [<issue> …]        batch — validate each, swap all to phase:ready over REST
/approve --sweep                    discover every Plan-complete issue, validate each, confirm, approve all
/approve <issue> --note "<text>"    posts the text as a comment on the issue
/approve <issue> --skip-adr         bypass the ADR-merged check (emergency override)
```

## Gate checks

Run all of these. **Refuse** if any fail; list every failure in the refusal output, don't stop at the first.

| Gate | Check | Refusal message |
|------|-------|-----------------|
| Phase | issue carries the `phase:plan` label | "Issue is at <current>, not Plan. Use `/scope` or `/bounce` first." |
| Problem statement | body has `## Problem statement` and the section is non-empty | "Missing or empty §Problem statement." |
| Design notes | body has `## Design notes` and is non-empty | "Missing or empty §Design notes." |
| Implementation plan | body has `## Implementation plan` and is non-empty | "Missing or empty §Implementation plan." |
| ADR merged | if §Design notes references an ADR PR, that PR's `mergedAt` is non-null (see [ADR gate, in detail](#adr-gate-in-detail); an ADR reference *also* forces the `human` tier via the [ADR hard gate](#adr-hard-gate)) | "ADR PR #M is not merged. Merge it or pass `--skip-adr` to override." |
| Model label | exactly one `model:*` label present, except a pure umbrella (non-empty `## Sub-issues`, coordination-only own plan; see [Multi-PR umbrella issues](#multi-pr-umbrella-issues)), which carries none (REST: `gh api repos/iamacoffeepot/aether/issues/<n>/labels`) | "Missing model:* label (or more than one). `/scope` stamps model routing at Plan — re-run its Plan step or add the label by hand." |
| Not blocked | no `blocked` / `wontfix` / `duplicate` label present | "Issue carries label '<label>' which blocks approval." |
| Freshness | targeted paths exist on `origin/main`; churn since scope re-grounds against the plan's anchors and surfaces only a broken one | "Targets removed on main: <paths>" (hard refuse) / "Plan anchor broken by churn — re-ground failed: `<pattern>` at `<path>`" (soft surface) |
| Dependency | every `#N` in `## Depends on` is a closed (Done) issue | "Blocked on unlanded dependency: #N (open)." |
| Umbrella integrity | if `## Sub-issues` is non-empty, the own `## Implementation plan` describes only coordination/integration, not net-new code the children don't cover | "Malformed umbrella: `## Sub-issues` plus a substantial own plan. Split the residual plan into its own child issue (leaving a pure umbrella), or remove `## Sub-issues` to make it a plain implementable issue." |

If **all** gates pass, decide the approval tier: the [ADR hard gate](#adr-hard-gate) first (an ADR-bearing issue routes to the owner unconditionally), then the [Approval tier](#approval-tier) policy lookup for everything else. The tier decides who approves; the gates above decide whether the issue is approvable at all.

## Freshness gate

Runs after the structural gates pass, against a freshly-fetched `origin/main`. Two tiers; both operate on paths extracted from §Implementation plan "files touched" segments and §Design notes §Affected surfaces.

**Tier A — target existence (hard gate).** For each extracted path, test `git cat-file -e origin/main:<path>`. If any path is absent on `origin/main`, the plan targets removed code — refuse with the missing paths listed. `git fetch origin main` first so the check uses the current remote state, not a stale local cache.

```bash
git fetch origin main
git cat-file -e origin/main:<path>   # exit 0 = exists, exit 128 = gone
```

A Tier A failure is a hard refusal (single) or drop-with-reason (sweep): the issue's premise is provably dead — there is nothing to implement.

**Tier B — drift since scope (auto re-ground).** Churn on a target since scope is not a park by itself — the machine re-runs the plan's anchors against current `main` and parks only when the churn actually broke an edit site. Plans are grep-anchored by convention (`/scope` §Plan: anchors + re-runnable patterns, not frozen line numbers), so the re-ground the park used to ask a human for is mechanical.

First establish the **drift floor**. A prior re-ground leaves a deduped `<!-- aether-agent:freshness-baseline sha=<origin/main-sha> -->` breadcrumb comment on the issue; when one is present, its sha is the floor — so a re-grounded issue is not re-detected as fresh drift every run. With no baseline marker yet, the floor is the scoped-at reference: the timestamp of the most-recent `phase:plan` labeled event on the issue timeline.

```bash
gh api repos/iamacoffeepot/aether/issues/<n>/timeline \
  --jq '[.[] | select(.event=="labeled" and .label.name=="phase:plan")] | last | .created_at'
```

Detect churn on the declared-surface paths (and each ADR file named in §Design notes) since the floor — a `git diff --name-only` against the baseline sha when a marker sets the floor, else `git log --since` against the scoped-at timestamp:

```bash
git diff --name-only <baseline-sha> origin/main -- <path>   # baseline-marker floor
git log origin/main --since=<scoped-at> -- <path>           # scoped-at floor (no baseline yet)
```

An empty churn set → no drift; proceed to tier resolution. A non-empty set means `main` has moved a target since the floor; re-ground it rather than parking:

1. Parse §Implementation plan for the edit sites whose file paths intersect the churned set.
2. Derive each site's **anchor** per the `/scope` §Plan convention — the explicit `git grep` / `rg` pattern where the plan gives one, else the cited symbol plus its path (`fn foo` in `<path>` → `git grep -n 'fn foo' origin/main -- <path>`).
3. Run each anchor against `origin/main` and check it **still lands** (a non-empty match).

**Every anchor still lands** → the drift is cosmetic. Refresh the freshness baseline: record the current `origin/main` sha in a `<!-- aether-agent:freshness-baseline sha=<sha> -->` breadcrumb comment (deduped — skip the post if a marker already carries this sha), then proceed to tier resolution with at most that breadcrumb — no park, and in sweep mode no digest freshness row.

**Any anchor is broken** (its pattern no longer matches, or the symbol is gone) → surface (single) / file a digest freshness row (sweep) naming the **specific broken anchor — its pattern and its path** — an actionable ask, not "targets churned, re-scope." The reviewer re-scopes the one broken edit site; `/implement` never inherits a dead anchor.

**A churned path whose plan site carries no machine-runnable anchor** (no pattern and no citable symbol) falls back to today's surface (single) / digest freshness row (sweep) for that path — conservative: only auto-clear drift the machine can actually verify.

This is the mechanism the former "Symbol-tier follow-on" note deferred: #2204 landed the stable-anchor + discovery-command plan convention, so re-running a plan site's anchor confirms the named target still exists on `origin/main`, not just the file — Tier A still guards raw file existence, Tier B now guards the edit sites themselves.

## Dependency gate

Runs after the structural gates pass. Parse every `#N` reference from the issue's `## Depends on` section (if the section is absent or empty, the gate passes trivially). For each referenced issue, read its state over REST:

```bash
gh api repos/iamacoffeepot/aether/issues/<N> --jq '.state'
```

Any dependency whose state is not `closed` is an unlanded blocker. A non-`closed` dependency is a hard refusal for a single `/approve` and a drop-with-reason for `--sweep` — the same semantics as a Tier A freshness failure. The refusal message names the blocking issue: `"Blocked on unlanded dependency: #N (open)."` List all open dependencies, not just the first. "Done" in this project means the issue is closed — consistent with the Backlog/Done = phase-label-absence/closed convention.

## Actions on pass

Runs once the structural gates pass *and* the [approval tier](#approval-tier) has cleared — `auto` clears on its own, `judge` and `human` clear on their approver's decision. The actions themselves are the same whoever approved:

1. Reconcile each approved issue's label to `phase:ready` (a REST `PUT …/labels` per issue, see [Phase label reconcile](#phase-label-reconcile)) — the `phase:ready` label is the canonical phase state and the agent-eligibility signal `/implement` reads. A single-issue `/approve` is the N=1 case — one label swap. When a batch mixes passing and failing issues, swap the label only for the ones that cleared every gate and list the rest in the refusal.
2. No comment on a plain approve — the `phase:ready` label and the timeline's label event already record it. If `--note` was passed, post the note as prose markdown:

   ```markdown
   **Approved** — <note text>
   ```

3. Print a summary to the user:

   ```
   ✓ #N approved.
   Phase: Plan → Ready
   Next: /implement <N>   (or wait for the orchestrator)
   ```

## Idempotency

If `/approve` is re-run on an issue that already carries `phase:ready`:

- Re-validate the gates (catches drift if anyone hand-edited the body).
- If gates still pass: no-op, print *"Already approved — Phase=Ready."* No new comment.
- If gates now fail: refuse and list failures. Don't auto-bounce — let the user decide whether to fix the body or `/bounce` the issue.

## Side findings

`/approve` is intentionally **not the place to triage side findings**. The §Side findings section is informational at this gate. Side findings get triaged via `/scope-spinoff <issue>` before or after approval — the user's call when. Approving an issue with un-triaged side findings is fine and common; the findings stay in the body for the next reviewer (or a future maintenance pass).

## Multi-PR umbrella issues

An issue carrying a non-empty `## Sub-issues` section is either a **pure umbrella** or a **malformed umbrella**:

- **Pure umbrella** — its `## Sub-issues` children collectively cover all implementation work. Its own `## Implementation plan`, if present, describes only coordination or integration steps (e.g. "merge children in order", "run the migration script once all pieces land"), not net-new code the children don't cover. `/approve` means "the overall plan is approved, children are split correctly". The umbrella itself is not `/implement`-able; leaving it at `Phase=Ready` is correct — it advances to `Done` only when every child is `Done`. Each child still goes through its own `/scope` → `/approve` flow. It carries no `size:*` or `model:*` label — `/scope`'s Plan phase exempts it from both, since it is never `/implement`-dispatched and there is nothing for either label to describe or route.

- **Malformed umbrella** — its own `## Implementation plan` describes net-new code or steps not delegated to any listed child. The Umbrella integrity gate (see [Gate checks](#gate-checks)) refuses this shape: the author must either split the residual plan into its own child issue (leaving a pure umbrella) or remove `## Sub-issues` to make it a plain implementable issue.

The one-or-the-other invariant means any issue that reaches `/implement` with a non-empty `## Sub-issues` is a pure umbrella and is correct to drop.

The agent tick owns the umbrella's end of life (#3212): its `phase:ready` arm never dispatches `implement` for a non-empty `## Sub-issues` issue — the umbrella rests until every listed child is closed, then the tick deletes its phase label and closes it as completed.

## ADR hard gate

Runs **before any policy lookup**, permanently and unconditionally. An ADR-bearing issue routes to the `human` tier — the owner — before [Approval tier](#approval-tier) is consulted at all. No policy rule, present or future, can auto- or judge-approve it: an ADR is load-bearing by definition. This rule lives in skill text, above the policy file, so it survives any edit to `.github/approval-policy.yml`; that file's `docs/adr/**` `human` entry is belt-and-suspenders, not the source of the rule. The [Approval tier](#approval-tier) lookup is consulted only for a non-ADR issue.

**An issue is ADR-bearing when it carries an explicit `ADR flag:` line, or its `## Declared surface` includes `docs/adr/`** — that is, when the work *needs* an ADR or *writes* one.

Citing an ADR in prose is **not** the test. `/scope` grounds every design in the ADRs it rests on — a good plan cites several — so a citation-based test routes the entire board to the owner and makes the policy file dead code. That is exactly what happened when this gate first shipped: every issue on the `phase:plan` board carried between three and fifteen ADR citations, none carried an `ADR flag:`, and all of them were short-circuited to `human` without the tier ever being resolved. The gate must separate *this design rests on ADR-0099* from *this work changes the architecture*; only the second is load-bearing.

Do not widen this back to a prose match. The safety property does not need it: anything that adds, changes, or is gated on an ADR still carries an `ADR flag:` (which `/scope` emits precisely for load-bearing work) or declares `docs/adr/` in its surface, and either one routes to the owner permanently.

Note the [ADR merge gate](#adr-gate-in-detail) is a *different* check answering a *different* question — whether an ADR this issue depends on has merged yet — and its citation-based parse is correct there. Routing and merge-readiness are not the same test; do not collapse them.

## Approval tier

For a non-ADR issue (the [ADR hard gate](#adr-hard-gate) has already routed an ADR-bearing one to the owner), resolve the issue's approval tier against `.github/approval-policy.yml` over its `## Declared surface` globs (the machine-readable glob block `/scope` emits at Plan):

1. Read the policy file's `default` tier and its ordered `rules` list of `{glob, tier}` entries; `tier ∈ {auto, judge, human}`.
2. Each declared path's tier is the **most restrictive** matching rule (`human > judge > auto`), or the file's `default` when no rule matches — today `judge`, so an unpoliced surface gets a second reader rather than a rubber stamp. Read the tier out of the file; do not assume it. Globs are gitwildmatch (gitignore-style `**`), matched exactly as the reconciler's containment step matches them.
3. The issue's approval tier is the most restrictive tier over every path in its declared surface.

Route by the resulting tier:

- **`auto`** — advance to `phase:ready` with no owner decision (the [Actions on pass](#actions-on-pass) label swap runs unchanged).
- **`judge`** — route to the LLM approval judge (#3133), shadow-mode first: the judge's verdict is recorded but the owner still confirms until the judge is trusted.
- **`human`** — hold for the owner's explicit `/approve`, exactly as today.

### The owner's override — `approval:pre-approved`

An issue carrying `approval:pre-approved` resolves to the **`auto`** tier whatever the policy says. It is the owner's way to say "I have read *this one*, let it go" without widening the policy for a whole surface.

Three constraints, and none of them are negotiable:

**It cannot pass the [ADR hard gate](#adr-hard-gate).** That gate runs *above* the policy lookup and *above* this label. An issue carrying an `ADR flag:` or declaring `docs/adr/` routes to the owner even with the label applied. An ADR is load-bearing by definition — there is no passable gate for one. Check the ADR gate first and refuse before you ever look at this label.

**It must be the owner's label.** Verify against the timeline: the most recent `labeled` event for `approval:pre-approved` must name the repo owner as its actor (the same check `approval:surface-ok` uses — `gh api repos/iamacoffeepot/aether/issues/<n>/timeline`). An agent may apply the label; it does not count, and the refusal says who applied it. Without this check an agent could grant itself unbounded approval authority through a side door, which is the one thing this whole policy exists to prevent. Never treat the label's mere presence as sufficient.

**It waives the tier, not the [gate checks](#gate-checks).** The tier answers *who approves*; the gate checks answer *whether the issue is approvable at all* — sections present, exactly one `model:*`, dependencies closed, targets still on `origin/main`. A pre-approved issue still runs every one of them and still refuses on a failure. A missing `model:*` means `/scope` did not finish, which is a defect to fix rather than something to wave into `implement`.

The tier decision runs *after* the structural [Gate checks](#gate-checks) pass — a failing gate refuses regardless of tier. Relaxing a tier is a one-line, owner-signed diff to `.github/approval-policy.yml`; the file's git history is the delegation-ladder audit trail. The declared surface this tier is resolved over is the same surface the reconciler later enforces the merged diff against, so the thing that ships is the thing that was approved.

## ADR gate, in detail

The [ADR hard gate](#adr-hard-gate) decides *routing* — an ADR-bearing issue always goes to the owner. This section is the *merge* check the structural gates run: an ADR the issue depends on must already be merged. Parse §Design notes for a URL or reference matching one of:

- `https://github.com/<owner>/<repo>/pull/<N>`
- `Closes <owner>/<repo>#<N>` (the cross-repo close form per the user's memory)
- A bare `#<N>` paired with an "ADR" mention nearby

For each such reference, read the PR's merge state over REST — `gh api repos/iamacoffeepot/aether/pulls/<N> --jq '.merged'` returns `true` once merged (the REST `state` only distinguishes `open`/`closed`, so `merged` is the field to test). Require it `true`; list every unmerged ADR PR in the refusal.

`--skip-adr` exists for cases where:

- The ADR is intentionally drafted in the same release but lands separately (e.g. ADR-NNNN cluster work).
- The change is small enough that ADR-by-the-time-Ready is overkill in retrospect.

When `--skip-adr` is used, a comment is mandatory — the override rationale has no structured home, and the next reader of the issue needs it:

```markdown
**Approved with `--skip-adr`** — ADR PR #M was not merged at approval time.

<required note text>
```

`--skip-adr` requires `--note "<reason>"`. Don't allow silent ADR bypasses.

## Phase label reconcile

The `phase:*` label is the canonical phase state — it is the only phase store the pipeline keeps, legible on the issue itself and discoverable over the REST issues endpoint. The swap rides REST: `gh issue edit --add-label/--remove-label` is GraphQL-backed, while the `gh api …/labels` endpoints are REST, so the phase write stays off the contended pool.

```bash
# Atomic swap to phase:ready. Runs under bash for array word-splitting.
bash <<'EOF'
n=<n>; new="phase:ready"; repo=iamacoffeepot/aether
args=()
while IFS= read -r l; do args+=(-f "labels[]=$l"); done < <(
  gh api "repos/$repo/issues/$n/labels" --jq '.[].name | select(startswith("phase:") | not)')
args+=(-f "labels[]=$new")
gh api -X PUT "repos/$repo/issues/$n/labels" "${args[@]}"
EOF
```

The single `PUT …/labels` replaces the label set with the non-`phase:*` labels plus `phase:ready`, so the issue never carries two phase labels and never carries zero — a tighter guarantee than a remove-then-add pair, which has a window between its two calls. The only write this skill makes is `phase:ready`; run the swap once per approved issue. On idempotent re-run (already Ready) the swap re-asserts the same set — a harmless no-op that also self-heals a hand-stripped label.

## Failure modes

- **GitHub API rate limit**: retry with backoff. If still failing, abort and tell the user the rate-limit reset time.
- **Hand-edits during validation**: if the issue body changes between the gate read and the label swap, re-read and re-validate before committing the phase-label transition. Don't write a partial transition.

## What `/approve` does NOT do

- Dispatch implementation. Run `/implement <issue>` (or wait for the Phase C orchestrator) after approval.
- Edit the issue body. Even if a gate fails because a section is missing, /approve doesn't write the missing section — that's `/scope`'s job.
- Auto-resolve side findings.
- Edit `.github/approval-policy.yml`. Relaxing a tier is the owner's signed diff, never a side effect of an approval run — an issue that wants a looser tier says so and waits for that diff to land.
- Enforce the declared surface against a PR's actual diff. `/approve` resolves the tier over the declared globs; the reconciler is what later holds a PR whose diff escapes them.
- Close umbrella issues when children complete. The agent tick's `phase:ready` arm owns that (#3212).
- Notify anyone. The printed summary (and the `phase:ready` label) is the surface; comments appear only for `--note` / `--skip-adr`.
