# Muse as the review finder, and the prompt defect it exposed

The question was whether Muse can take the review finder seat, since review is the
long pole of a bloom and Muse is roughly twice as fast per call. Measured against
the `spike/review-calibration` dataset, the answer arrived in two parts, and the
second one matters more than the first.

**Muse under the current production finder prompt recalls 83.3% (standard deviation
8.3 percentage points over five trials) against Sonnet's 91.7%.** On its own that
reads as a straightforward "no".

**Four edits to the finder prompt take Muse to 12/12 on three of three trials —
zero variance, zero false positives, no latency cost.** An ablation with every
answer-shaped word removed scores identically, so this is a real defect in the
prompt rather than the answer key smuggled into the instructions. The gap that
looked like a model-quality gap was substantially a prompt gap, and the prompt is
shared by every finder on the panel.

## Method

Dataset, answer key, seeded tree, and finder prompts are reused verbatim from
`spike/review-calibration`: sixteen items over two lenses — twelve seeded defects
graded L1 (blatant) through L4 (domain knowledge required), plus four clean
false-positive controls. Ten defects are synthetic; two (`r1_config_cap`,
`r2_fuel_order`) are real bugs previously fixed in this repository and re-seeded by
reverting the fix, with their regression tests removed from the tree.

Both arms ran the production finder prompt with read-only file access to a checkout
of the dataset tree state, plus an identical appended JSON output contract. Muse ran
through `muse exec` (`muse-spark-1.2-contributor`); Sonnet through `claude -p`
(sonnet, effort high). Items ran one at a time within a cell so every duration is a
clean per-call figure. The grading hit rule is ported verbatim from the original
sweep: a finding matches when its symbol or current form contains the key's function
name, or its line falls within ten lines of the key's line.

Harness validation: Sonnet scored 11/12 here with the same single miss
(`s8_pan_law`) as the original sweep, which used a different harness with a forced
output schema. Different harness, same number.

## The reasoning-effort ladder

| effort  | recall            | false positives | mean   | median | max    |
|---------|-------------------|-----------------|--------|--------|--------|
| minimal | 7/12  (58.3%)     | 0               | 12.7s  | 11.8s  | 31.9s  |
| low     | 8/12  (66.7%)     | 1               | 18.6s  | 16.4s  | 32.3s  |
| medium  | 83.3% over 5 runs | 0               | 24.5s  | 22.9s  | 61.9s  |
| high    | 9/12, 10/12       | 0               | ~27.0s | ~24.9s | 44.4s  |
| xhigh   | 10/12 (83.3%)     | 0               | 39.0s  | 39.9s  | 58.9s  |

Baseline: Sonnet at high effort, 11/12 (91.7%), 0 false positives, mean 46.6s,
max 170.1s.

The ladder is flat above `low` — medium, high and xhigh all land near 83%, and a
single trial per cell cannot resolve differences smaller than the roughly eight
percentage points of run-to-run noise a twelve-item dataset carries. Medium is the
cheapest point on that plateau rather than a peak: it matches xhigh's recall at
about sixty percent of the latency. The only real step on the ladder is `low` to
`medium`. The value `none` appears in `muse exec --help` but is rejected at runtime
(exit code 2), so that row is absent; `ultra` was not run.

## Five trials at medium

| trial | recall        | false positives | mean  | max   | misses                                     |
|-------|---------------|-----------------|-------|-------|--------------------------------------------|
| t1    | 11/12 (91.7%) | 0               | 23.6s | 34.6s | s5_bounds_order                             |
| t2    | 9/12  (75.0%) | 0               | 24.5s | 61.9s | s5_bounds_order, s8_pan_law, r2_fuel_order  |
| t3    | 10/12 (83.3%) | 0               | 24.3s | 37.9s | r1_config_cap, r2_fuel_order                |
| t4    | 9/12  (75.0%) | 0               | 25.1s | 48.7s | s7_ewma, s8_pan_law, r2_fuel_order          |
| t5    | 11/12 (91.7%) | 0               | 25.0s | 43.7s | r1_config_cap                               |

Pooled: 83.3%, standard deviation 8.3 percentage points, range 75.0–91.7%.

## What it misses, and why

Three measurements narrow the failure mode considerably.

**It is not skimming.** All ten misses returned zero findings — it never once
pointed at the wrong place — and misses took slightly *longer* than hits on the same
item (plus 0.2 to 2.2 seconds). Precision stayed intact throughout: zero findings and
zero lint candidates on all four clean controls across all five trials.

**Its own confidence field predicts the misses.** Every one of the thirty-five hits
on never-dropped items came back at `confidence: high`. On the items it sometimes
drops, only sixty percent were high confidence, and not one was ever rated
`severity: high`. It never dropped anything it would have called high-confidence,
high-severity. The misses are the tail of a self-declared reporting threshold, which
also explains why they look stochastic.

**Twice it diagnosed a defect correctly and then filed it elsewhere.** On
`s5_bounds_order` it named the wrong expression, the correct one, the disagreeing
sibling match arm, and the comment that still described the old behavior — then
routed the whole thing to `lintCandidates` on the grounds that it was *"not a
MISSING BOUNDS CAP shape under this lens"* and *"still bounded by input length"*.

The content pattern behind which items it drops is referent locality, not difficulty.
The seven it never drops all have a visibly wrong token inside the diff: `let _ =` on
a `Result`, `.unwrap()` in a function that otherwise returns `?`, `.max(height)` where
`padded_height` is in scope, a `continue` ahead of the push, a dirty flag hoisted above
its guard, an assertion against its own literal, a `decode(encode(x))` over plain
derives. The five it drops all look plausible on screen and are wrong only against a
referent defined elsewhere:

| item | catch rate | wrong only against |
|---|---|---|
| s7_ewma | 4/5 | the intended formula — needs algebra, not reading |
| s5_bounds_order | 3/5 | `remaining()` versus `bytes.len()` semantics on the cursor type |
| s8_pan_law | 3/5 | the specification fixing 127, not 128, as the maximum |
| r1_config_cap | 3/5 | nothing on screen at all — the cap is *absent*, and a sibling path has one |
| r2_fuel_order | 2/5 | a zero-fuel store plus the single call path that supplies it |

Subtlety level does not predict this. `s6_leak` is L3 and 5/5; `s4_silent_noop` is a
cross-restructure L2 and 5/5; meanwhile L3 `r1_config_cap` is flaky. What predicts it
is whether the judgment closes inside the diff.

The sharpest version: the two items drawn from real fixed bugs are its two worst, at
50% (5/10) against 90% (45/50) on the synthetic ones, and Sonnet caught both. Two
items is a direction rather than a rate, but the mechanism is not a coincidence. A bug
that survives into a codebase is selected for not being locally visible — had it been
visible, review would have caught it. A finder that decides locally will therefore
score better on a synthetic benchmark than it will in the seat, and any future
calibration number should be read with that bias in mind.

## The prompt fix

Each edit answers one of the failures above. `prompts/v3/` holds the full text and
`prompts/v3.diff` the exact change against the production prompt in `prompts/v1/`.

**A — the shape list read as a closed whitelist.** *"flag only these"* became *"stay
inside this lens"*, plus: these shapes are the lens's focus, not an exhaustive
checklist; if the change makes the code behave contrary to its own contract and the
defect is nearest to one of these shapes, report it under that shape; never withhold a
behavioral defect you can name because it is an imperfect fit for a shape's wording.

**C — the `lintCandidates` chute was catching contract-level judgment.** Narrowed to
only what a linter could decide with no knowledge of what the code is supposed to do,
with two additions: if you had to reason about the code's own contract to conclude it
is wrong, it is a finding, even when a lint fires nearby; and routing a defect you have
already diagnosed to `lintCandidates` is a miss, not caution.

**D — "be precise and conservative" was suppressing the medium-confidence band.**
Conservative now means not inventing a misbehaving path you cannot name, and explicitly
not withholding a defect you can name because its blast radius is small, its severity
low, or your confidence merely medium. Report at the confidence you actually hold.

**E — report scope was reading as look scope.** The instruction restricting findings to
the file under review now says that this restricts where you report and never where you
look, and adds: before you conclude a file is clean, state to yourself what the changed
lines depend on being true, and go check it. This is the edit aimed at the eight silent
misses.

A fifth edit widening the `MISSING BOUNDS CAP` shape to cover a bound computed from the
wrong quantity was tested and then dropped — see the ablation.

## Ablation

| arm | prompt | trials | recall | standard deviation | false positives | mean |
|---|---|---|---|---|---|---|
| v1 | production | 5 | 83.3% | 8.3pp | 0 | 24.5s |
| v2 | five edits, two answer-shaped | 3 | **100%** | 0.0 | 0 | 26.9s |
| v3 | four edits, no answer-shaped text | 3 | **100%** | 0.0 | 0 | 24.9s |

The first pass (`v2`, in `prompts/v2.diff`) included an edit that nearly quoted two of
the dataset's defects, and gave edit E a list of examples drawn from the misses. That
makes its score uninterpretable on its own. `v3` drops the widened bounds shape entirely
and reduces edit E to *"read whatever you need in order to judge THIS change; before you
conclude a file is clean, state to yourself what the changed lines depend on being true,
and go check it"* — nothing naming any defect in the set. It scored the same, at
baseline latency, so the gain is a genuine prompt fix.

Every previously flaky item went to 3/3 under both variants, including `r2_fuel_order`
at 2/5 in the baseline.

## Before adopting this

Three things this does not establish, worst first.

**Precision is the open risk.** Four clean controls across three trials is twelve
observations, nowhere near enough to detect a small rise in the false-positive rate, and
edits C and D both push toward reporting more. The review funnel works because false
positives run at roughly one in 280 calls. A prompt that moved that to one in forty
would be a net loss whatever recall says. This needs a clean-heavy dataset before the
change goes anywhere near production.

**The dataset is saturated.** At 12/12 the variants are indistinguishable and there is
no visible headroom. The edits were also written with knowledge of this set's failure
modes, so even the deliberately generic prose was aimed. Held-out defects are the honest
test.

**It is untested on other models.** Sonnet misses `s8_pan_law` consistently under the
production prompt. If edits C and E lift Sonnet too, this stops being a story about Muse
and becomes a panel-wide prompt improvement — in which case the seat comparison needs
redoing on the fixed prompt before anyone concludes anything about seats.

Two smaller notes. There is no trustworthy cost comparison here: the Claude command-line
interface self-reported 6.56 US dollars for its sixteen calls and `muse exec` reports no
token counts in plain mode, and those figures are not comparable anyway. And Sonnet has
one trial in this harness against Muse's five, so the headline gap is a five-trial mean
measured against a two-point estimate whose own variance is unmeasured.

## Reproducing

The harness expects two environment variables: `MUSE_SEEDED_TREE` pointing at a checkout
of the dataset tree state (the `dataset tree state` commit in the
`spike/review-calibration` history), and `MUSE_RUN_DIR` pointing at this directory.
Prompts reference the seeded tree as `<WORKTREE>`; substitute the real path before use.

```
MUSE_RUN_DIR=. TRIAL=t1 python3 harness/muse_sweep.py medium   # baseline trial
PROMPTS=prompts/v3 TRIAL=v3a python3 harness/muse_sweep.py medium
python3 harness/compare.py                                     # all arms side by side
python3 harness/trials.py                                      # per-trial grading
python3 harness/pattern.py                                     # miss shape and durations
python3 harness/confidence.py                                  # the threshold check
python3 harness/misses.py                                      # demoted diagnoses
```

`harness/variant.py` and `harness/variant_v3.py` regenerate the two edited prompt sets
from `prompts/v1/`, so the diffs are checkable rather than trusted.

## Layout

```
harness/     runner, graders, prompt-variant builders, the answer key
prompts/v1/  the production finder prompt, as run
prompts/v3/  the fixed prompt
prompts/*.diff   both variants against v1
results/     per-call records (JSON Lines) and rendered tables
```

Key result files: `prompt_ablation.txt` (all eleven trials side by side),
`medium_trials.txt` (the five baseline trials), `miss_pattern.txt` (the three miss
analyses), `muse_matrix.txt` (the effort ladder).
