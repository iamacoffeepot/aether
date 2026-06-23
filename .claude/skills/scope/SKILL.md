---
name: scope
description: Walk a GitHub issue Define → Design → Plan and reconcile its phase:* label. Stops at Plan, awaiting user approval via /approve. Aggressive (agent makes design calls) and resumable (per-phase label updates).
---

# /scope — release scoping skill

Walk a single issue from Backlog through Define → Design → Plan, producing a problem statement, design rationale, and implementation plan as structured body sections. Reconciles the issue's `phase:*` label as it advances. Stops at `Plan`, awaiting user review via `/approve`.

This skill produces **scoping artifacts only**. It does not write production code (that's `/implement`), open implementation PRs, or advance the issue to Ready (that's `/approve`).

## Invocation

```
/scope <issue-number>                fresh run, or resume from current Phase
/scope <issue-number> --phase define rewrite Define section, redo downstream
/scope <issue-number> --phase design rewrite Design section, redo Plan
/scope <issue-number> --phase plan   rewrite Plan section only
/scope --sweep                       discover every Backlog issue, one scoper agent each, confirm, run
/scope --sweep <issue> [<issue> …]   sweep an explicit set instead of discovering
/scope --sweep --model <m>           route the scoper agents to model <m> (default opus)
```

## Sweep dispatch

`/scope --sweep` is the batched discovery entry point: instead of one issue, it takes the Backlog set, dispatches one background scoper agent per issue, and waits for your confirmation before any agent spawns. It is the read-only twin of `/implement --sweep` — each agent runs a full single-issue `/scope` (Define → Design → Plan, writing that issue's body sections and its `phase:*` / `size:*` / `model:*` labels) in its own context, then stops with a bounded summary. The parent assembles the batch, confirms, paces dispatch, and rolls up the outcomes.

Two things differ from `/implement --sweep`:

- **The agent owns its issue's whole scope.** `/implement` keeps the serial tail (push, PR, CI loop) in the parent because that tail is flaky; scope has no such tail — no worktree, no push, no CI — so each scoper agent writes its issue's artifacts directly and there is nothing to hand back. Each agent writes only its own issue's `phase:*` label transitions plus its other labels; the cross-agent pacing is the concurrency cap below, not a shared batch.
- **Two models, not one.** The scoper agents run a design-capable model — `--model` (default `opus`), because scoping makes design calls. That is distinct from the per-issue `model:*` label each agent *stamps* at Plan to route the future `/implement`.

1. **Enumerate the candidate set.** With no issue arguments, discover every Backlog issue. Backlog is the resting phase that carries no `phase:*` label (see [Phase label reconcile](#phase-label-reconcile)), so an open non-PR issue with no `phase:*` label is at Backlog. Over REST (per [GitHub API budget](#github-api-budget) — not `gh issue list`, which is GraphQL-backed):

   ```bash
   gh api 'repos/iamacoffeepot/aether/issues?state=open&per_page=100' \
     --jq '.[] | select(has("pull_request") | not)
                | select([.labels[].name] | any(startswith("phase:")) | not) | .number'
   ```

   With explicit arguments (`/scope --sweep 1815 1816 …`), take that set verbatim instead of discovering.

2. **Gate-check each candidate.** Each must be at Backlog (no `phase:*` label) and carry a body framable into a problem statement (the bar Define applies). Drop a candidate already past Backlog (it carries a `phase:*` label) or with a one-line, context-free body, and record the reason; the sweep never silently skips — every dropped issue is listed in the plan with its drop reason.

3. **Print the scope plan and wait for confirmation.** Print the issues to be scoped, the routed scoper model, and the dropped-with-reason list, then stop and wait:

   ```
   Sweep: 9 Backlog issues, 2 dropped, 7 to scope (scoper model: opus).

   Scope (Backlog → Plan):
     #1815  add xl size option for fat issues
     #1816  /sweep fat recursive issue decomposition
     …

   Dropped:
     #1799  one-line title, no framable body — /sketch it further first
     #1803  phase:design, already past Backlog

   Confirm scope? (the agents spawn only on your go-ahead)
   ```

4. **On confirmation, dispatch.** Spawn one background agent per candidate, capped at **4** concurrent agents — the binding constraint is concurrent design-agent compute (scope runs Opus) and roll-up legibility, not the REST pool (a scope agent issues ~10 REST calls over several minutes, far below the 5,000/hr budget at any plausible concurrency): fill the cap, and as each agent finishes the next candidate starts. Each agent runs the full single-issue `/scope` procedure on its issue and stops; the parent collects each outcome — landed at Plan, or self-bounced — and prints the roll-up.

The sweep never auto-confirms. A scoper agent that hits a tied design decision or an unframable body self-bounces its own issue (the single-issue Bounce mechanics), and the parent surfaces that bounce in the roll-up rather than guessing. `--sweep` takes no `--phase` (that is a single-issue resume control).

## Grounding against `origin/main`

This section is the canonical grounding rule for the whole pipeline — `/implement` routes its reads by it. Every `/scope` and `/implement` run reads code from a working filesystem, but that filesystem can lag `origin/main`: a role-bound session worktree (ADR-0110) is branched at session start and never auto-synced, so it drifts behind as sibling PRs land, and a dispatched per-agent worktree can be cut before a sibling PR merges. Either way an agent that reads the lagging tree grounds its plan against code that has already changed on `origin/main`, producing a wrong scope or a wrong plan. Ground every read against `origin/main`, by these three rules:

1. **Sync before reading.** Whoever holds the worktree — the session for an in-place run, or the parent before it dispatches agents — fetches and fast-forwards the clean tree first: `git fetch origin main` then `git merge --ff-only origin/main`. A dirty or diverged tree that cannot fast-forward is surfaced, not forced — report it and stop rather than papering over real divergence.

2. **Verify, then ground claims against the ref.** As step 1 of any scope or implement read, verify the tree is current — `git rev-parse HEAD` equals `git rev-parse origin/main`. Ground every `file:line` claim against the `origin/main` ref directly (`git grep <pat> origin/main -- <path>`, `git show origin/main:<path>`), authoritative whatever the worktree HEAD happens to be.

3. **A surprise is staleness first.** An "extra" call site, an "untracked surface", or a site the issue body never listed is a staleness signal before it is a defect. Diff that path against the ref (`git diff --name-only HEAD origin/main` over it) to confirm the tree is current, and only then escalate the finding as a real defect or a §Side finding.

Both execution contexts ground by these rules: the role-bound session worktree (the common path for a scoper) and the dispatched per-agent worktree (the common path under `--sweep`). The worked example is the case that motivated the rule — a session worktree one commit behind `origin/main`, where that one commit had deleted the exact call sites a child issue targeted, so a scoper reading the lagging tree produced a false "untracked surface" finding and a wrong plan grounded on code that no longer existed on main.

These are the *read-side* discipline — the counterpart write-side mirror is the Plan edit-site citation convention (see `### Plan` Output block): cite each edit site by stable anchor and re-runnable pattern so the next reader can re-ground cheaply rather than trusting a frozen snapshot.

## Phase walk

For each sub-phase: read inputs, write the corresponding body section, and reconcile the issue's `phase:*` label (see [Phase label reconcile](#phase-label-reconcile)). If a sub-phase has nothing to do (already complete on a resumed run), skip it.

### Define

- **Inputs**: issue body, comments, linked issues (`gh issue view <n> --json body,comments,closingIssuesReferences`).
- **Output**: replaces or adds `## Problem statement` section in the body. Two short paragraphs:
  1. *What's being solved.* The concrete problem in plain language.
  2. *Why now / success criteria.* Why this release; what "done" looks like observably.
- **Bounce**: if the body is too vague to frame a problem (e.g. one-line title, no description, no linked context), self-bounce to `phase:bounced` with a `bounce-to:define` label and a comment asking the specific clarifying question. Don't guess.
- **Phase label**: reconcile to `phase:design` on success.

### Design

- **Inputs**: this issue's body so far, related ADRs in `docs/adr/`, the auto-memory directory, relevant code in the affected crates (read via Read tool, not just grep).
- **Posture**: aggressive — the agent picks between roughly-equal options and notes the rejected one. Self-bounce only when options are truly tied *or* the right answer needs information only the user has.
- **Output**: replaces or adds `## Design notes` section. Structure:
  ```
  ### Chosen approach
  <one paragraph: what we'll do and why>

  ### Rejected options
  - **<option A name>** — why not (one line)
  - **<option B name>** — why not (one line)

  ### Affected surfaces
  <crates, public traits, wire formats, ADRs touched>
  ```
- **Cross-crate type move — cycle-direction check**: when the chosen approach moves a type from a lower crate into a higher one (lower→higher in the crate dependency DAG), the scoper must not assert cycle-freedom — it must record a concrete inverse-direction check in `### Affected surfaces`. Because a lower crate cannot depend on a higher one, the hazard for a lower→higher move is an item *remaining* in the lower crate that still references the moved type; grep the lower crate against `origin/main` for consumers of the moved type (e.g. `git grep <TypeName> origin/main -- crates/<lower-crate>/`) and paste the command and its result into §Design notes. An empty consumer set is the evidence the move is cycle-free; a non-empty set means the move closes a dependency cycle and the plan must resolve it (move the consumer too, relocate to a third crate, or abandon the move).
- **ADR drafting**: if the chosen approach is load-bearing (touches public traits, wire formats, lifecycle, dispatch, or otherwise looks architectural), scaffold an ADR via the `/adr` skill on a `docs/adr-NNNN-<slug>` branch and open a PR. Link the ADR PR in the Design notes section. The issue is not eligible for `/approve` until the ADR PR is merged.
- **Bounce**: rare. Use only when truly stuck on a value-judgment the user must make.
- **Phase label**: reconcile to `phase:plan` on success.

### Plan

- **Inputs**: this issue's body (Define + Design must be present), the affected files (deeper read than Design — look at the actual code that will change).
- **Output**: replaces or adds two sections:

  ```
  ## Implementation plan

  1. <step> — <files touched> — <test coverage>
  2. <step> — ...
  ```

  Edit-site citation convention: locate each single edit site by a stable anchor — the symbol it touches (`fn foo`, `struct Bar`) and its file path; a line number, if given at all, is an advisory navigation hint (`near fn foo`) and is never the load-bearing locator. Express each multi-site change (importers, callers, a test module set) as the `git grep`/`rg` pattern that enumerates it — name the pattern and the expected shape of matches rather than a frozen count, so the implementer re-runs it against current `main` and acts on the complete present-day set. Example (stale): *"Update the 3 importers of `OldType` in `crates/aether-data/` (lines 12, 47, 88)."* Example (stable): *"Update every importer of `OldType` in `crates/aether-data/` — `git grep 'OldType' origin/main -- crates/aether-data/`; act on the current match set."*

  And, if the work spans multiple PRs:

  ```
  ## Sub-issues

  - #NNN <child issue title>
  - #MMM <child issue title>
  ```

- **Multi-PR split** triggers when:
  - More than 3 logically-separable changes, *or*
  - More than 2 crates with logically-separable work
  - In that case, file each sub-issue via `/sketch`'s mechanics (filed at Backlog — no `phase:*` label — linking the parent), update §Sub-issues, and scope each child in a follow-up `/scope` run.
- **Size estimation** — stamp the issue's `size:*` label by this rubric:
  - **S**: single file, single concept, <100 LOC change
  - **M**: single crate, multiple files, <500 LOC change
  - **L**: cross-crate, architectural, or >500 LOC change

  Write it as a `size:s|m|l` label over REST (the same `gh api …/labels` endpoints the [Phase label reconcile](#phase-label-reconcile) uses — never `gh issue edit`, which rides GraphQL), so the estimate the dispatcher reads lives entirely off the contended pool:

  ```bash
  gh api "repos/iamacoffeepot/aether/issues/<n>/labels" --jq '.[].name | select(startswith("size:"))' \
    | while read -r l; do gh api -X DELETE "repos/iamacoffeepot/aether/issues/<n>/labels/$l"; done \
    && gh api -X POST "repos/iamacoffeepot/aether/issues/<n>/labels" -f "labels[]=size:<x>"
  ```
- **Model routing (required)** — every issue leaves Plan with exactly one `model:*` label; `/approve` refuses an unlabelled issue. The label names the model the dispatched agent runs — there is no inherit-the-dispatcher fallback. Stamp it over the same REST `…/labels` endpoints as the size mirror, in the same step. Pick by the work, defaulting cheap:
  - `model:haiku` — trivial text-only work: doc links, label fixes, one-line config tweaks.
  - `model:sonnet` — the default for mechanical work fully specified by the plan (typically S, and an M whose plan reads as executable verbatim).
  - `model:opus` — the implementation itself requires non-obvious judgment: cross-crate L work, design-adjacent changes, plans with open exploration.
  - `model:fable` — never stamped by `/scope`; reserved for a human pinning the top tier explicitly.

  The gate encodes a size-asymmetry: a downward misjudgment costs more as size grows, so an M/L `model:sonnet` demands a plan that reads as executable verbatim — when in doubt at M or L, stamp `model:opus`. Note the choice and a one-clause reason inline in the `## Implementation plan` section.
- **Phase label**: leave at `phase:plan`. This is the resting state awaiting `/approve`.

## Side findings

During Design and Plan reads, the agent will inevitably notice unrelated issues — dead code, undocumented invariants, latent bugs in adjacent files. Don't chase them. Add a body section:

```
## Side findings

- <one-line description> — <pointer: file:line or crate>
- ...
```

These are *not auto-filed* as child issues. The user reviews them at `/approve` time and chooses which to spin off via `/scope-spinoff <issue>` (separate skill).

## Comments

No progress comments — phase transitions are already legible from the `phase:*` labels (the issue timeline records every label change) and the body sections themselves. A comment exists only when it is addressed to a human and carries content with no structured home. For this skill that is the **self-bounce question/blocker**, written as prose markdown with a bold lead, no `[skill]` prefix. (The model-routing rationale is recorded inline in `## Implementation plan` — it has a structured home in the body, so it never goes in a comment.)

```markdown
**Bounced to Define** — the body doesn't say which chassis this applies to.

Desktop-only (window mail exists) or all four? The answer changes whether the
headless chassis needs a nop mailbox. Re-run `/scope` once the body says.
```

Don't pad the comment with summaries of work that completed — the body sections carry that.

## Body editing mechanics

A body edit replaces the entire body, so to avoid clobbering user-written content:

1. Read the current body — `gh api repos/iamacoffeepot/aether/issues/<n> --jq '.body'`.
2. Identify scope-managed sections by their H2 headers: `## Problem statement`, `## Design notes`, `## Implementation plan`, `## Sub-issues`, `## Side findings`. Everything else is user content; preserve verbatim.
3. Insert or replace the managed sections, preserving user content above and below them.
4. Write back over REST — `gh issue edit --body` is GraphQL-backed, while `PATCH …/issues/<n>` is REST. Write the new body to a file first so its backticks / `$` aren't shell-expanded: `gh api -X PATCH repos/iamacoffeepot/aether/issues/<n> -F body=@/tmp/issue-<n>-body.md`.

The agent must be careful here — the body is the canonical scope artifact and clobbering user-written background notes will erode trust.

## GitHub API budget

This section is the canonical GitHub API-budget reference for the whole pipeline — the other skills route their `gh` calls by it. GitHub meters REST and GraphQL on separate 5,000-point/hr budgets per user, and a batch run drains the GraphQL pool while the REST pool sits idle. The convenience `gh` subcommands (`gh issue create`, `gh issue edit`, `gh pr create`, `gh pr merge`, `gh pr list`, `gh pr checks`) are all GraphQL-backed, so every op with a REST endpoint goes through its `gh api` form. The pipeline runs almost entirely on REST: phase state is a `phase:*` label (the [Phase label reconcile](#phase-label-reconcile) swap), and the size / model / type metadata are labels too, so there is no project board to write. The lone GraphQL-only op left is the PR un-draft (`markPullRequestReadyForReview`), once per PR at land time. `{owner}` is the hardcoded `iamacoffeepot`; the repo is always `aether`.

### REST-vs-GraphQL routing

Every op with a REST endpoint rides REST (`gh api <path>`); only the one PR-draft op in the GraphQL-only list below stays on GraphQL.

| Op | REST form (`gh api …`) |
|----|------------------------|
| Create issue | `-X POST repos/{owner}/aether/issues -f title=… -f body=… -f 'labels[]=type:x' -f 'labels[]=crate:y'` |
| Edit issue body | `-X PATCH repos/{owner}/aether/issues/{n} -f body=…` |
| Comment | `-X POST repos/{owner}/aether/issues/{n}/comments -f body=…` |
| Add label | `-X POST repos/{owner}/aether/issues/{n}/labels -f 'labels[]=…'` (adds; does not replace other labels) |
| Swap label set | `-X PUT repos/{owner}/aether/issues/{n}/labels -f 'labels[]=…' …` (replaces the whole set atomically — the [Phase label reconcile](#phase-label-reconcile) form) |
| Remove one label | `-X DELETE repos/{owner}/aether/issues/{n}/labels/{label}` |
| Read labels | `repos/{owner}/aether/issues/{n}/labels --jq '.[].name'` |
| List issues by label | `'repos/{owner}/aether/issues?labels=…&state=…' --jq '.[].number'` |
| Open PR (draft) | `-X POST repos/{owner}/aether/pulls -F draft=true -f title=… -f head=… -f base=main -f body=…` |
| Merge PR | `-X PUT repos/{owner}/aether/pulls/{n}/merge -f merge_method=squash` |
| Read PR / merge state | `repos/{owner}/aether/pulls/{n} --jq '.state, .merged, .merged_at'` (REST is snake_case) |
| List PRs by head | `'repos/{owner}/aether/pulls?head={owner}:{branch}&state=…' --jq '.[].number'` |
| CI check-runs | `repos/{owner}/aether/commits/{sha}/check-runs --jq …` (the standing CI-monitor rule) |

**GraphQL-only — no REST equivalent:**

- Un-draft a PR — `markPullRequestReadyForReview` (the REST `pulls` PATCH cannot clear `draft`). The sole GraphQL op the pipeline still issues, in `/land`; everything else, phase state included, is REST.

## Phase label reconcile

The `phase:*` label is the canonical phase state — the only phase store the pipeline keeps, legible on the issue itself and discoverable over the REST issues endpoint. The swap rides REST: `gh issue edit --add-label/--remove-label` is GraphQL-backed, while the `gh api …/labels` endpoints are REST, so the phase write stays off the contended pool.

```bash
# Atomic swap to an active phase. Runs under bash for array word-splitting.
bash <<'EOF'
n=<n>; new="phase:<new>"; repo=iamacoffeepot/aether
args=()
while IFS= read -r l; do args+=(-f "labels[]=$l"); done < <(
  gh api "repos/$repo/issues/$n/labels" --jq '.[].name | select(startswith("phase:") | not)')
args+=(-f "labels[]=$new")
gh api -X PUT "repos/$repo/issues/$n/labels" "${args[@]}"
EOF
```

The single `PUT …/labels` replaces the label set with the non-`phase:*` labels plus the one new `phase:*`, so the issue never carries two phase labels and never carries zero — the atomic write is a tighter guarantee than a remove-then-add pair, which has a window between its two calls. A failed PUT leaves the prior labels untouched and heals on the next run. For this skill the writes are `phase:design`, `phase:plan`, and (on self-bounce) `phase:bounced`. Two phases carry no label — `Backlog` (the resting/default state) and `Done` (the issue is closed); to move to either, delete the present phase label instead of swapping:

```bash
gh api "repos/iamacoffeepot/aether/issues/<n>/labels" --jq '.[].name | select(startswith("phase:"))' \
  | while read -r l; do gh api -X DELETE "repos/iamacoffeepot/aether/issues/<n>/labels/$l"; done
```

## Restart and resume semantics

- **Fresh `/scope <issue>`**: detect the current phase from the issue's `phase:*` label. Run only the sub-phases that haven't completed. A completed sub-phase is one whose body section is present and non-empty.
- **`/scope <issue> --phase <name>`**: force rewrite of that sub-phase's section regardless of completion. Downstream sub-phases re-run because their inputs changed. (E.g. redoing Design implies redoing Plan because Design choices drive Plan steps.)
- **After a bounce**: the user resolves the bounce (clarifies the issue, picks the tied option), then re-invokes `/scope <issue>` to resume from the bounced phase.

## What `/scope` does NOT do

- Write production code (use `/implement` after `/approve`).
- Open implementation PRs (use `/implement`).
- Merge anything.
- Auto-file side findings as child issues (use `/scope-spinoff`).
- Advance the issue to Ready (use `/approve`).
- Stamp `Type` — `/sketch` sets the `type:*` label at filing from the title's conventional-commit prefix; issue metadata rides labels, so there is nothing further for `/scope` to stamp here.

## Failure modes to handle gracefully

- **Issue already closed (Done) or carrying `phase:executing`**: refuse with *"Issue is past Plan — use `/bounce` to regress or work in a fresh issue."*
- **ADR drafting failure**: keep the issue at Design, explain in the run's output, don't advance to Plan.
- **Body-edit collision (user edited body mid-run)**: re-read, re-merge, surface any conflicts in managed sections in the run's output.
