# Offline quality eval

The **quality eval** is the fleet's ground-truth correctness regression alarm. CI
gates syntax, types, lints, and the tests a change ships; it cannot see whether a
routing, effort, or model change quietly made the agents *worse at solving
problems*. The dominant failure class the fleet produces — semantic-invariant
bugs, wrong edge-case behavior, silently dropped requirements — passes every
mechanical gate. The quality eval is the standing signal for that class.

It works by re-implementing recently-landed issues **blind** and scoring the
result against what actually merged. A weekly workflow samples ~5 issues, runs a
fresh agent against each one with no access to the merged solution, and a judge
compares each blind candidate diff to the landed "ground truth" diff. The output
is a verdict-rate table grouped by `size:*` and `model:*` — the axes a routing
change moves — posted onto one rolling tracking issue so the trend is visible
week over week. It runs off the merge critical path and never gates a PR.

The probe arc this productizes (2026-07-13/14) already found that effort level
barely moves correctness below `size:l`, that Opus-medium beats Sonnet-xhigh on
`size:l`, and that the failure class is invisible to CI. The eval turns that
one-off finding into a repeatable measurement.

## What "ground truth" means

The landed squash commit is the ground truth. It merged, so it passed review and
CI and represents an accepted solution to the issue. A blind re-implementation is
scored **against** it: not "does it match byte-for-byte" — a different but
functionally equivalent implementation is *correct* — but "does it solve the
issue with the same behavior, free of the checklist's semantic-defect classes".

## The three stages

The harness is three `scripts/` stages plus the weekly workflow. Each stage
streams newline-delimited JSON to the next, so they compose in a pipe and each is
independently runnable from a fixed input.

### 1. Select — `scripts/quality-eval-select.mjs`

Enumerates merged PRs from the trailing window (`QUALITY_EVAL_WINDOW_DAYS`,
default 7) that closed a **code-bearing** issue (the squash commit touched a
crate source file). For each, it resolves the closing squash commit and its
**parent SHA** — the pre-merge trunk tip the blind run clones at — and captures,
*at selection time*, the closing issue's `size:*` / `model:*` labels and its body.
It then deterministically samples `QUALITY_EVAL_SAMPLE_SIZE` (default 5) and emits
one record per line:

```json
{"issue":123,"pr":456,"squash_sha":"…","parent_sha":"…","model_label":"model:opus","size_label":"size:l","issue_body":"…"}
```

The sample is deterministic (ordered by a stable hash of each candidate's SHA) so
a re-run over the same candidate set reproduces the same sample. The body is the
runner's task input and the labels feed the judge's aggregation — neither is
produced by any later stage, so both are captured here.

### 2. Run — `scripts/quality-eval-run.sh`

The blind runner. Per sample it builds a **sealed scratch repo** and runs an
isolated coding agent against the issue body, then harvests the candidate diff
beside the landed ground-truth diff, emitting:

```json
{"issue":123,"candidate_diff":"…","landed_diff":"…","model":"opus","model_label":"model:opus","size_label":"size:l"}
```

Blindness is **structural**, not a matter of trusting the agent:

- **Single-revision clone.** `git clone --revision=<parent_sha> --depth=1` (git
  ≥2.49) makes `parent_sha` the *only* reachable commit — the landed squash
  commit is reachable from no ref in the scratch repo.
- **Sealed remote.** The clone's `origin` is removed immediately, so the agent
  cannot `git fetch` trunk even on a public repo.
- **Contamination assert.** Before the agent runs, `git rev-list --all` must not
  contain the squash SHA. A hit fails that sample loudly rather than reporting a
  possibly-peeked verdict — a contaminated scratch would silently inflate the
  "correct" rate, the exact failure this design guards against.
- **Edit-only isolation.** The agent runs under an `--allowedTools` allowlist
  (Read / Edit / Write / Grep / Glob / Bash for build+test — no `WebFetch`, no
  MCP, no `gh`) with **no** GitHub token in its env. This is a belt on top of the
  structural seal, not the primary guarantee.

The no-token / no-`gh` constraint scopes to the *coding-agent invocation only* —
the driver script itself uses git and the network to clone and to read the landed
diff from the full-history checkout.

### 3. Judge — `scripts/quality-eval-judge.mjs`

Drives a judge model over each record, comparing the candidate diff to the landed
diff under a fixed known-failure-mode checklist (semantic-invariant, boundary,
error-handling, incomplete, wrong-default, regression). It parses a structured
per-sample verdict (`correct` / `defect`, with a defect class) and aggregates
verdict rates **grouped by `size_label` and `model_label`**, printing the rollup
markdown. The defect rate is computed over *scored* (correct + defect) samples —
an unparseable verdict is counted but never scores as correct.

Only the pure logic is unit-tested (`scripts/quality-eval.test.mjs`): the
selector's trailing-window filter and deterministic sampler, and the judge's
verdict parser and rate aggregation (with a tripwire on the aggregated rates).
The git isolation and the `claude` invocations are I/O, verified by the
contamination assert at runtime rather than by a test.

## The weekly schedule

`.github/workflows/quality-eval.yml` runs on a weekly `schedule` (Mondays, off the
other nightlies' minutes) and posts the verdict-rate rollup onto a single rolling
tracking issue titled **"quality-eval: weekly offline verdict rates"** — a comment
each week on the same issue, so the trend reads top to bottom. The workflow's
token is `contents: read` + `issues: write` (the latter only for that tracking
issue); the blind agent runs with neither.

## Running it manually

Dispatch the whole harness on demand, optionally overriding the sample size:

```bash
gh workflow run quality-eval.yml -f sample_size=3
```

Or run the stages locally (needs `git ≥2.49`, `node`, `jq`, and a
`CLAUDE_CODE_OAUTH_TOKEN`):

```bash
GITHUB_TOKEN=$(gh auth token) node scripts/quality-eval-select.mjs > samples.jsonl
QUALITY_EVAL_CLONE_TOKEN=$(gh auth token) bash scripts/quality-eval-run.sh samples.jsonl > judged.jsonl
node scripts/quality-eval-judge.mjs judged.jsonl        # prints the rollup
```

## Reading the verdict rates

The rollup reports, per `size` × `model` cell and overall, how many blind
re-implementations were judged, how many were correct, how many carried a defect,
and the defect rate over the scored samples, followed by a per-defect list naming
the issue and its defect class. A defect rate that climbs after a routing / effort
/ model change — especially on `size:l`, where the probe arc found the model
choice matters most — is the signal that the change degraded correctness in a way
no other check would surface. A single week is noisy at five samples; the value is
the trend across weeks on the rolling issue.
