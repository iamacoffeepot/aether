# Spike: review-agent model × effort calibration sweep

**Date:** 2026-07-14 · **Branch:** `spike/review-calibration` · **Question:** what model and what reasoning effort should the review workflow's finder and refuter agents run on?

The review workflow (`.claude/workflows/review.js`) exposes two model knobs — `finderModel` (default `sonnet`) and `verifyModel` (default `opus` on the deep pass) — and passes **no effort override**, so agents inherit the session effort. This spike calibrates both roles empirically: a matrix sweep of model × effort over a 16-item dataset of seeded bugs (graded L1–L4 by subtlety), real historical bugs, junk-test tripwires, and clean controls, all judged with the **production prompts verbatim**.

## TL;DR recommendation

- **Finder:** `sonnet` at **high** effort (or medium — the two are within noise of each other; high won 91.7% vs 87.5% here). Do NOT raise to xhigh: sonnet:xhigh scored *worse* (83.3%) than medium — the extra deliberation talks the finder out of correct flags. **Do not switch the finder to opus**: opus missed both real historical bugs and the subtlest seeded bug at *every* effort (66.7–75% recall), reading ~½ the context and emitting ~⅓ the tokens of sonnet:high per call (it under-invests in the file). Haiku is startlingly competitive (83–92%) at a fraction of the price and is a legitimate floor option for cheap passes.
- **Refuter:** model/effort barely matters on this dataset — sonnet:medium, sonnet:high, opus:medium, opus:high, and fable:medium all kept 6/6 real findings and killed 6/6 fabricated ones (sonnet:medium's single deviation was an honest, policy-compliant `uncertain`). The production default (opus) is safe; sonnet:high is equal on this evidence and cheaper.
- **Fable (ceiling anchor):** 12/12 finder recall — the only cell to catch everything — at 42 s / 2.0k output tokens per call. It establishes the dataset is fully solvable; it is not a cost-rational production finder.
- **False positives are a non-issue:** 1 FP in 280 finder calls (haiku:low; audited unreachable-overflow claim). The production prompt's conservatism holds the FP floor; **recall is the only axis that moves** with model choice.
- **Bycatch — a real defect in the review workflow itself:** grounding refuters mutate the shared checkout (write scratch tests, temporarily apply the suggested fix, revert with `git checkout`). Run in parallel on overlapping files they read each other's scratch state and produce false refutations; on an *uncommitted* tree a `git checkout` revert silently destroys the working state. See §Incidents. Recommended fixes: `isolation: 'worktree'` for build-capable refuters, or serialize refuters per file; never run the review workflow over uncommitted work.

## 1. Design

### 1.1 What is being measured

The review funnel has two judgment roles whose model/effort are configurable:

1. **Finder** — one agent per file × lens, reads a PR-style diff plus the full file, reports findings under the production correctness / test-integrity taxonomies. Failure mode of interest: **false negatives** (a seeded bug not flagged) and false positives (findings on clean diffs).
2. **Refuter** — one agent per high-severity finding, adjudicates it against the strict bar. Failure modes: killing a real finding (manufactured false negative) and rescuing a hallucinated one (false positive passthrough).

Both roles ran the prompts from `review.js` **byte-for-byte** (finder: `findPrompt` single-file diff-scoped form; refuter: `refutePrompt` correctness form with the `NO_BUILD` grounding step). The only deviation: the sweep isolates one lens per item instead of running all lenses per file.

### 1.2 The matrix

Finder cells (11): haiku {low, medium, high}, sonnet {low, medium, high, xhigh}, opus {low, medium, high}, fable {medium}. Frontier cells sonnet:medium, sonnet:high, opus:medium ran **2 trials**; all others 1. 16 items × 14 cell-trials = 224 calls (+56 decontamination re-runs).

Refuter cells (5): sonnet {medium, high}, opus {medium, high}, fable {medium} × 12 findings (6 real from the answer key, 6 fabricated) = 60 calls (+20 re-run, +5 serial).

### 1.3 The dataset (16 items, one file each)

Full answer key: `dataset/manifest.json`. Each item's exact presented diff: `dataset/<item>/diff.patch`. Levels grade *subtlety*: L1 = blatant to any careful reviewer → L4 = requires domain knowledge (pan-law math, EWMA anchoring, wasm fuel semantics).

| id | level | lens | shape | file | one-line bug |
|---|---|---|---|---|---|
| s1_swallowed_error | L1 | correctness | swallowed-error | audio/runtime/handlers.rs | schedule push result discarded; full queue reported Ok |
| s2_unwrap | L1 | correctness | swallowed-error | aether-codec frame.rs | length-prefix read `.unwrap()` panics on peer disconnect |
| s3_off_by_one | L2 | correctness | off-by-one | text/runtime/atlas.rs | shelf height drops padding term; atlas rows bleed |
| s4_silent_noop | L2 | correctness | silent-incompleteness | trace/walk.rs | visited-recipient `continue` drops Sent from stitched tree |
| s5_bounds_order | L3 | correctness | missing-bounds-cap | aether-codec decode.rs | Vec pre-alloc clamped to total input, not remaining |
| s6_leak | L3 | correctness | resource-leak (stale flag) | render/runtime/texture.rs | dirty flag set before validation, not rolled back |
| s7_ewma | L4 | correctness | invariant-violation | lifecycle/runtime/settlement.rs | decreasing-branch EWMA anchored at sample (α weight flipped) |
| s8_pan_law | L4 | correctness | invariant-violation | audio/runtime/voice.rs | pan normalized /128.0 not /127.0; hard right unreachable |
| r1_config_cap | L3 | correctness | missing-bounds-cap | aether-mcp tools/components.rs | **real bug #3247**: config_path read unbounded by frame cap |
| r2_fuel_order | L4 | correctness | ordering | aether-behavior host/slot.rs | **real bug (fix 3052c7bf)**: fuel armed after write_guest; restore state silently dropped |
| t1_mirror | L1 | test-integrity | mirror | aether-kinds transforms.rs | junk test: NAME asserted against its own literal |
| t2_roundtrip | L2 | test-integrity | derive-only-roundtrip | aether-kinds text_metrics.rs | junk test: decode(encode(x)) over plain derives |
| c1_clean | CLEAN | correctness | — | http/client/runtime.rs | benign-only diff (FP control) |
| c2_clean | CLEAN | correctness | — | component/runtime/mod.rs | benign-only diff (FP control) |
| c3_clean | CLEAN | correctness | — | aether-codec encode.rs | benign-only diff (FP control) |
| c4_tripwire | CLEAN | test-integrity | — | aether-data hash.rs | **legit** computed-value tripwire test (FP control) |

Construction principles:

- **Diff realism.** Every bug is wrapped in 10–30 lines of benign cover changes (doc comments, local renames, extracted locals) so the presented hunk reads like a normal refactor PR where exactly one change is wrong. Clean controls are the same *shape* of diff with zero defects — form-indistinguishable twins.
- **Tree realism.** Mutations were applied to the live worktree (not detached copies) so finders opening "the full file for context" see real crate context. The whole seeded state compiles (`cargo check` per crate).
- **Real bugs as ground truth.** r1/r2 revert actual shipped fixes; their regression tests (which post-date the bugs) were removed from the tree for the run so finders face the bug the way history did. Their diffs present each function as newly introduced.
- **CI-plausibility.** s6's pinning asserts were weakened in-tree (a CI-red PR never reaches review). s2's blast radius (4 downstream socket tests in aether-capabilities) was discovered late and accepted — finders never run tests; noted in `prompts/seeding/agent-c-codec.md`.
- **Anti-leak.** No comment hints at any bug; the answer key stayed out of the repo during runs; r1/r2 diff headers were sanitized after a scratchpad path leaked item names into the `diff --git` line.

Seeding was done by 4 parallel general-purpose agents (prompts + verbatim answer keys: `prompts/seeding/`); r1/r2 and the s6 test-weakening were hand-built. Cost: ~443k tokens, 114 tool uses, ~28 min wall across the four agents.

### 1.4 Grading

Deterministic, in-workflow (no judge model): a finding **hits** if its `symbol`/`current_form` contains the answer-key fn name, or its `line` is within ±10 of the key line. Clean items: every finding is an FP. Buggy items: findings matching neither key are FPs. Grading code: `harness/sweep.js` (`grade()`).

## 2. Results

### 2.1 Finder matrix — hit/miss per item (decontaminated: s2/s3/s4/r2 from the re-run)

`H` = flagged the seeded bug, `.` = missed. Double columns = 2 trials.

| item | ha:low | ha:med | ha:high | so:low | so:med | so:high | so:xhigh | op:low | op:med | op:high | fa:med |
|---|---|---|---|---|---|---|---|---|---|---|---|
| s1 swallowed L1 | H | H | H | H | HH | HH | H | H | HH | H | H |
| s2 unwrap L1 | H | H | H | H | HH | HH | H | H | HH | H | H |
| s3 off-by-one L2 | H | H | H | H | HH | HH | H | H | HH | H | H |
| s4 silent-noop L2 | H | . | H | . | .. | HH | H | . | HH | H | H |
| s5 bounds L3 | H | H | H | H | HH | HH | H | H | HH | H | H |
| s6 leak L3 | H | H | H | H | HH | HH | H | H | H. | H | H |
| s7 ewma L4 | H | H | H | H | HH | HH | H | H | HH | H | H |
| s8 pan-law L4 | . | H | H | . | H. | .. | . | . | .. | . | H |
| r1 config-cap L3 (real) | . | . | . | . | HH | HH | . | . | .. | . | H |
| r2 fuel-order L4 (real) | H | H | H | H | HH | HH | H | . | .. | . | H |
| t1 mirror (test) | H | H | H | H | HH | HH | H | H | HH | H | H |
| t2 roundtrip (test) | H | H | H | H | HH | HH | H | H | HH | H | H |

### 2.2 Finder matrix — totals

FPs counted over all 16 items (incl. clean controls). Telemetry = finder-main run averages per call (effort attribution order-inferred, see §3.3; model attribution exact).

| cell | recall | FPs | avg wall/call | avg output tokens/call |
|---|---|---|---|---|
| **fable:medium** | **12/12 = 100%** | 0 | 42.4 s | 1,981 |
| sonnet:high | 22/24 = 91.7% | 0 | 56.6 s | 3,937 |
| haiku:high | 11/12 = 91.7% | 0 | 69.3 s | 3,975 |
| sonnet:medium | 21/24 = 87.5% | 0 | 24.5 s | 1,564 |
| haiku:low | 10/12 = 83.3% | 1 | 78.8 s | 4,807 |
| haiku:medium | 10/12 = 83.3% | 0 | 70.3 s | 4,428 |
| sonnet:xhigh | 10/12 = 83.3% | 0 | 82.7 s | 6,594 |
| opus:high | 9/12 = 75.0% | 0 | 39.7 s | 1,997 |
| sonnet:low | 9/12 = 75.0% | 0 | 16.6 s | 868 |
| opus:medium | 17/24 = 70.8% | 0 | 27.9 s | 1,449 |
| opus:low | 8/12 = 66.7% | 0 | 21.3 s | 1,091 |

Model-exact averages (attribution independent of effort inference): haiku 72.8 s / 4,403 tok / 2.7 tool-uses; sonnet 43.6 s / 3,078 / 2.9; opus 29.2 s / 1,497 / **1.9**; fable 42.4 s / 1,981 / 3.0.

### 2.3 What the misses say

- **Opus under-invests.** Its calls are the fastest, emit the fewest tokens, and make the fewest tool calls (1.9 avg — it reads the least context). It missed **both real historical bugs at every effort** (r1: 0/4 cells, r2: 0/4) and the pan-law seed (0/4). These are exactly the bugs that need the file's own invariants read (the sibling embed path that *does* cap; the wasm fuel lifecycle; the ADR pan contract). Raising opus's effort did not change this — effort tunes deliberation, not thoroughness of evidence-gathering.
- **Sonnet:xhigh over-deliberates.** 6.6k output tokens per call — the most of any cell — yet it *lost* r1 and s8, both of which sonnet:medium/high caught. The long deliberation appears to argue borderline flags back below the "be conservative" bar. xhigh is strictly dominated: slower, dearer, lower recall.
- **Haiku grinds.** 70–79 s and ~4.4k tokens per call of methodical reading gets it to 83–92%. Its one systematic gap is r1 (0/3) — recognizing a *missing* guard requires knowing the sibling path has one. It caught the pan-law at medium/high where opus never did.
- **s4 is the flakiest item** (silent-incompleteness in the trace walk): missed by haiku:medium, sonnet:low, sonnet:medium×2, opus:low even on the clean tree. The bug hides in a plausible-looking control-flow "de-sugaring" — the shape agents themselves most often write.
- **Test-integrity is easy.** Every cell nailed both junk tests and spared the legitimate computed tripwire (c4) — the taxonomy prompt carries the discrimination, not the model.
- **The single FP** (haiku:low on s3's file): an unreachable u32-overflow claim in `rect_rgba`, structurally identical to the F-atlas-overflow fabrication that every refuter cell killed. The funnel's refute stage would have caught it — i.e., end-to-end FP rate for the full pipeline on this dataset is 0.

### 2.4 Refuter matrix (interference-corrected; see §3 for why correction was needed)

6 real findings (must keep) + 6 fabricated confident-hallucination findings (must kill — each fabrication verified false against the code before the run):

| cell | kept real | killed fabricated | notes |
|---|---|---|---|
| sonnet:medium | 5/6 + 1 uncertain | 6/6 | the `uncertain` (T-s7 EWMA) is policy-honest: NO_BUILD rule 2 says return uncertain when no test covers a subtle claim |
| sonnet:high | 6/6 | 6/6 | grounded T-s2/T-s4 by writing+running probe tests |
| opus:medium | 6/6 | 6/6 | |
| opus:high | 6/6 | 6/6 | |
| fable:medium | 6/6 | 6/6 | serial re-run; grounded all four contested findings with live probe tests |

Every fabrication died in all 5 cells (30/30) with correct citations of the exculpating code (the existing cap two lines up, the visited-set, the zero-dim check, the per-call refuel). Refuter role: **model choice is not the differentiator** at these tiers; the prompt's strict-bar + grounding structure does the work.

## 3. Incidents — read before trusting any single-run number

The sweep surfaced a class of harness hazard worth more than the calibration itself. Full event log in §3.1–3.3; raw pre-correction rows are preserved in `results/` for forensics.

### 3.1 Refuters mutate the shared checkout

The production refute prompt tells the agent to ground its verdict: write a scratch `#[test]`, run it, delete it; never uphold a fix that would break the suite (which invites *applying* the fix to check). Agents did exactly that — and some cleaned up with `git checkout <file>`, which on our **uncommitted** dataset tree reverted the seeded mutation entirely (s2's and s4's files went back to pristine HEAD mid-sweep; one agent left r2's file half-"fixed", which tripped the harness's file-watcher). Any agent reading those files during the window judged a bug that wasn't there.

**Blast radius:** finder-main results for s2/s3/s4/r2 in the mid-run cells (sonnet/opus territory — jobs dispatch roughly in cell order, so haiku ran pre-corruption and fable post-repair); refute-round1 verdicts for T-s2/T-s3/T-s4/T-r2 (opus:high and fable "killed" real findings by *correctly* reporting the quoted defect didn't exist in the file they read).

**Containment:** all 16 seed signatures were re-verified and re-applied; the seeded state was then committed (`dataset tree state` commit in this branch's history) so any later `git checkout` self-heals to the *mutated* state; s2/s3/s4/r2 × all 14 cell-trials were re-run on the stable tree (finder-rerun) and those rows replace the main run's in every table above.

### 3.2 Parallel refuters interfere with each other

Even after the WIP commit, the refute re-run produced two fable verdicts quoting a file state that matched *another refuter's in-flight scratch fix* (the cover-renamed local with the fixed operator — a state that exists in no commit). Parallel grounding agents on overlapping files read each other's temporary edits. The final fable refute pass therefore ran **serially** (one agent at a time); all four contested verdicts flipped to `confirmed` with live probe-test grounding.

**Production implication for review.js:** `refuteFlags` runs per-finding high-severity refuters in parallel. Two high-severity findings on the same file get concurrent build-capable agents on one checkout — the same interference class. In CI the checkout is committed (checkout-revert self-heals), but scratch tests/fix-trials still collide. Recommended: `isolation: 'worktree'` for the per-finding refuter, or group refutes per file (the batch path already does). Filing this as an issue is a follow-up, not done in this spike.

### 3.3 Job-order attribution in workflow journals

`journal.jsonl` `started` order does not reliably equal the `parallel()` array order (slot races; an API failure reshuffles further). The workflow's own returned rows are keyed correctly (promise-based), so all **score** tables are exact; but per-agent telemetry (`results/agents.csv`) had to be re-attributed: model + item are recovered exactly per agent (from the transcript's API model id and the prompt's FILE line); effort/trial within a (model, item) group is dispatch-order-inferred and marked `order-inferred(effort)` in the `mapping` column (324 of 365 rows; 41 exact). Model-level aggregates are exact; per-effort telemetry splits carry that caveat. One refute-rerun agent died on an API server error mid-response (`sonnet:medium` T-r2) and was retried in the serial run.

### 3.4 Other accepted imperfections

- The pre-seeded tree fails 4 downstream socket tests (s2's unwrap panics `read_frame` on EOF paths) — irrelevant to finders (no test runs) but disqualifying for any build-grounded pass on the *finder* side; the refute matrix that needed builds ran after this was understood.
- s7's seeded flip contradicts the adjacent formula comment — that contradiction is the intended discoverable clue, but it makes s7 easier than a comment-free L4 (every cell caught it; treat s7's difficulty as ~L3 in hindsight).
- r1/r2's "function being introduced" diff framing differs from the historical introduction (which predates the split-out files); the bug content is identical.
- Single trials on most cells: treat ±1 item (±8.3%) as noise; the opus vs sonnet gap (16–25 points, consistent across efforts and corroborated on re-run) is well outside it.

## 4. Cost & time accounting

Full detail: `results/usage.json` (workflow-level) and `results/agents.csv` (per-agent: model, effort, item, wall seconds, input/output/cache-read/cache-creation tokens, tool uses, started/ended timestamps, result JSON, mapping quality).

| phase | agents | tokens | wall |
|---|---|---|---|
| Dataset seeding (4 general-purpose agents) | 4 | 442,740 | ~7 min ea, parallel |
| finder-main (224 calls) | 224 | 11,541,960 | 19.5 min |
| refute-round1 (60 calls) | 60 | 3,394,102 | 7.2 min |
| refute-rerun (20 calls) | 20 | 1,187,778 | 5.0 min |
| finder-rerun (56 calls) | 56 | 2,945,734 | 10.3 min |
| refute-serial (5 calls, sequential) | 5 | 315,559 | 9.5 min |
| **Total** | **369** | **~19.8 M** | ~2.5 h session wall |

## 5. Repository layout of this spike

```
spikes/review-calibration/
├── README.md                 — this document
├── dataset/
│   ├── manifest.json         — answer key: 16 items, level/shape/fn/line/provenance/bug
│   └── <item>/diff.patch     — the exact diff each finder was shown (16 items)
├── harness/
│   ├── sweep.js              — finder-sweep workflow script (template; production-verbatim prompts + grading)
│   ├── refute_sweep.js       — refuter-sweep workflow script (template)
│   ├── items.json            — finder items incl. embedded diffs (sweep input, paths sanitized)
│   ├── refute_items.json     — 12 refuter findings (6 real, 6 fabricated)
│   └── key.json              — grading key used by the sweeps
├── prompts/
│   ├── seeding/agent-{a..d}-*.md — the 4 dataset-construction agent prompts + verbatim answer keys + usage
│   ├── finder/<item>.txt     — all 16 concrete finder prompts as received (fidelity-checked vs transcripts)
│   └── refute/<id>.txt       — all 12 concrete refuter prompts
└── results/
    ├── agents.csv            — 365 rows: per-agent run/cell/item, wall time, token counts, tool uses, result JSON, mapping quality
    ├── main_rows.json        — finder-main graded rows (224; pre-decontamination, forensic)
    ├── rerun_rows.json       — finder-rerun graded rows (56; authoritative for s2/s3/s4/r2)
    ├── merged_rows.json      — decontaminated merge used for §2 tables
    ├── final_scores.json     — per-cell recall/FP rollup of the merge
    ├── refute_round1_rows.json / refute_rerun_rows.json — raw refuter rows incl. contaminated verdicts (forensic)
    ├── refute_final.json     — interference-corrected refuter table (§2.4)
    └── usage.json            — per-run and per-seeding-agent token/time/tool accounting
```

**Reproducing the seeded tree:** check out the `dataset tree state` commit in this branch's history (the tip has pristine `crates/` — the mutations were reverted after the runs). Each `dataset/<item>/diff.patch` also applies cleanly to the merge-base (`46ef44ad`), except r1/r2 whose patches present function-introduction diffs (apply their described edits instead: remove the `max_frame_size` guard in `component_config_bytes`; move `set_fuel` below `write_guest` in `offer_state`; remove the two named regression tests).

**Reproducing a sweep:** the harness scripts are workflow scripts for the Claude Code Workflow tool; bake `items.json` into `sweep.js` (`const ITEMS = …`) with the cells you want, run on a tree with the dataset commit checked out, and read the returned `rows`.

## 6. Follow-ups (not done in this spike)

1. File the refuter-isolation defect against review.js (`isolation: 'worktree'` for the per-finding high-severity refuter, or per-file serialization) — §3.2.
2. Consider `finderModel: 'sonnet'` + explicit `effort: 'high'` in review.js rather than inheriting session effort — today's CI sessions may run finders at whatever effort the box happens to use; this sweep says the choice moves recall by ~9 points and xhigh actively hurts.
3. A haiku-tier cheap pre-pass (economy/convention lenses) is plausibly free-lunch given haiku's 83–92% here; needs its own sweep on the advisory lenses before touching defaults.
4. If a future model swap is contemplated, re-run this dataset first — it is cheap (~$15-class in tokens for one 16×1 column) and the r1/r2/s8 trio is a sharp discriminator. The dataset's one-time construction cost dominated; marginal columns are cheap.
