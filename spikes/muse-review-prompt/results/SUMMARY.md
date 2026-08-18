# Muse vs Sonnet on the bloomery review finder

Dataset, prompts, answer key and seeded tree reused verbatim from
`spike/review-calibration` (seeded tree = commit a80004e46). Both arms ran the
production finder prompt with read-only file access on the seeded worktree and
an identical appended JSON output contract. Muse via `muse exec`
(muse-spark-1.2-contributor); Sonnet via `claude -p` (sonnet, effort high).
One item at a time within a cell, so every duration is a clean per-call figure.

Harness validation: sonnet:high scored 11/12 here, the same figure and the same
single miss (s8_pan_law) as the original agent()-based sweep. Different harness,
same number.

## Headline

Muse at its best effort level is **83.3% recall (sd 8.3pp) over five trials**,
against Sonnet's 91.7%. It is roughly 2x faster per call and 5x faster on the
tail. Its misses are stochastic rather than a fixed blind spot, which is the
part that makes it unsuitable as a straight swap.

## Recall x effort (Muse), one trial per cell except medium

| effort  | recall            | FP | mean   | median | max    |
|---------|-------------------|----|--------|--------|--------|
| minimal | 7/12  (58.3%)     | 0  | 12.7s  | 11.8s  | 31.9s  |
| low     | 8/12  (66.7%)     | 1  | 18.6s  | 16.4s  | 32.3s  |
| medium  | 83.3% over 5 runs | 0  | 24.5s  | 22.9s  | 61.9s  |
| high    | 9/12, 10/12       | 0  | ~27.0s | ~24.9s | 44.4s  |
| xhigh   | 10/12 (83.3%)     | 0  | 39.0s  | 39.9s  | 58.9s  |

Baseline: sonnet:high 11/12 (91.7%), 0 FP, mean 46.6s, max 170.1s.

`none` is not accepted by `muse exec --reasoning-effort` (rc=2); row dropped.
`ultra` was not run.

**The ladder above `low` is flat within noise.** medium, high and xhigh all sit
at ~83%. An earlier reading of this table called medium a non-monotonic peak;
that was a single-trial artifact, and five trials dissolve it. What survives is
that medium is the cheapest point on the plateau — same recall as xhigh at 60%
of the latency — and that the real step is `low` -> `medium`, not anything above.

## Medium, five trials

| trial | recall        | FP | mean  | median | max   | misses                                     |
|-------|---------------|----|-------|--------|-------|--------------------------------------------|
| t1    | 11/12 (91.7%) | 0  | 23.6s | 23.4s  | 34.6s | s5_bounds_order                             |
| t2    | 9/12  (75.0%) | 0  | 24.5s | 22.9s  | 61.9s | s5_bounds_order, s8_pan_law, r2_fuel_order  |
| t3    | 10/12 (83.3%) | 0  | 24.3s | 23.1s  | 37.9s | r1_config_cap, r2_fuel_order                |
| t4    | 9/12  (75.0%) | 0  | 25.1s | 22.6s  | 48.7s | s7_ewma, s8_pan_law, r2_fuel_order          |
| t5    | 11/12 (91.7%) | 0  | 25.0s | 25.1s  | 43.7s | r1_config_cap                               |

Pooled: **83.3%, sd 8.3pp, range 75.0-91.7%**. The 91.7% that appeared to tie
Sonnet is the top of the range, drawn twice in five.

t1 was measured with bloomery slots active; t2-t5 ran on an idle host. Recall is
unaffected; the latency columns are not strictly comparable across that line,
though they moved less than 2s.

## Per-item catch rate (5 trials)

| caught | items |
|--------|-------|
| 5/5    | s1_swallowed_error, s2_unwrap, s3_off_by_one, s4_silent_noop, s6_leak, t1_mirror, t2_roundtrip |
| 4/5    | s7_ewma |
| 3/5    | s5_bounds_order, s8_pan_law, r1_config_cap |
| 2/5    | r2_fuel_order |
| 0/5    | — |

Everything L1/L2 is 5/5. Every unreliable item is L3 or L4. Muse finds nothing
Sonnet cannot; it finds the harder half of the set only sometimes.

Sonnet's profile is the opposite shape: 11/12 on both observations, missing
s8_pan_law both times. A fixed blind spot can be patched with a lens or a second
pass. A stochastic one cannot — the same reviewer on the same code gives a
different answer each run, so nothing downstream can be calibrated against it.

## False positives

Zero across all five medium trials — 60 bug-item calls and 20 clean-control
calls, every control clean every time. Consistent with the original spike's ~1
in 280 FP rate. Precision is not where Muse loses.

## Reading

- Straight swap of the finder seat to Muse costs ~8pp of recall and adds run-to-
  run variance the funnel has no way to absorb.
- Union of muse:medium and sonnet:high is 12/12 on any given pair of runs, but
  the union is only as stable as its weaker member — the items Muse contributes
  (s8_pan_law) it hits 3/5.
- The latency win is real and effort-independent: max 32-62s across every Muse
  cell against Sonnet's 170s. Finder fan-out is parallel, so pass latency is the
  max, roughly a 5x cut on the critical path.
- The shape that would exploit that without paying the recall: Muse as a cheap
  wide pre-pass whose output is additive, with Sonnet still the seat of record.
  Not measured here.

## Caveats

- Sonnet has 1 trial in this harness (2 counting the original sweep, same score
  and same miss both times); Muse has 5 at medium. The comparison is a 5-trial
  mean against a 2-point estimate, and Sonnet's own variance is unmeasured.
- No trustworthy cost comparison. Claude CLI self-reported $6.56 for 16 calls;
  muse exec reports no tokens in plain mode. Money needs the sealed PriceTable
  over measured tokens.

## Artifacts

- results/medium_trials.txt, medium_trials.json -- five-trial grading
- results/muse_matrix.txt -- effort matrix
- results/muse-medium-t{1..5}.jsonl, muse-<effort>.jsonl -- per-call records
- results/sonnet.jsonl -- Sonnet baseline records
- raw/<arm>/<item>.stdout -- full model output per call
- muse_sweep.py (TRIAL=tN env for repeat runs), run_arm.py, matrix.py,
  trials.py, grade.py -- the harness
