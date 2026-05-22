# ADR-0085: Noise-aware perf comparison for scheduler changes

- **Status:** Proposed
- **Date:** 2026-05-22

## Context

Scheduler / dispatch changes — the dispatch-latency line of work (route-path folding, spin-park, the local-stickiness cap) — are evaluated with the lifecycle latency harness (`lifecycle_latency_observe`, the non-perturbing harness that drives the real lifecycle and harvests the resident trace ring). The dominant difficulty is not measuring a run; it is that **run-to-run noise swamps the signal**:

- the parked-worker wakeup fires unpredictably, so tail percentiles (`p99`, `max`) swing 100-1000x run to run;
- `p50` drifts with thermal / background load across a multi-cell sweep, so the config that happens to run *last* looks uniformly slower across *every* topology — including ones the change provably cannot touch;
- a single run cannot separate a real sub-microsecond or tail change from a fluke.

Concretely, in the local-stickiness sweep the fan-out win (tail latency 5-17x lower once the local cap reached the fan-out width) was real and well above noise, but an *apparent* chain regression was uniform run-order drift, and a 592us chain `max` was a one-off outlier. Every one of those calls was made by hand. We want a repeatable, trustworthy way to say "this scheduler change improved / is flat / regressed, beyond noise" — both in the harness and on PRs — without the verdict being a fresh judgment exercise each time. The companion build ticket is the harness comparison mode + the on-PR report.

## Decision

Perf comparison for scheduler changes is **noise-aware by construction**, on these rules:

1. **Full-run replication for the band.** Run the harness K times per configuration, each in a fresh process (the isolation that reproduces timing variance, same rationale as the flake-soak harness). The error bar is the spread of the per-trial percentile *across* trials — not the within-run sampling spread, which is tight but measures the wrong thing and understates uncertainty by orders of magnitude. Each trial's percentile already carries its own within-run error, so replicating full runs folds in both noise sources.

2. **Robust center and band.** Run-to-run is heavy-tailed (a trial can hit a multi-millisecond outlier), so the center is the **median of the per-trial percentiles** and the band is the **IQR** (or a bootstrap CI of the trial-median) — never the mean.

3. **Same-runner, interleaved, paired deltas.** Baseline and candidate run on the *same* runner, interleaved trial-by-trial (base, candidate, base, candidate, ...). The verdict is computed on the **per-trial delta** `delta_t = candidate_t - base_t`, not on two independent clouds — the shared runner drift cancels in each delta, so the band on the *change* is far tighter than on either absolute number. A cell is flagged improved or regressed only when the `{delta_t}` distribution clears zero by more than `effect-floor x band` (a paired nonparametric test), so a statistically detectable but trivially small change reads as stable.

4. **Informational, not gating.** The comparison produces a report (a sticky PR comment, and the harness's own output); it does not block a merge. Promotion to a soft gate happens only after the band is demonstrably trustworthy on real history.

5. **Cost-bounded trigger.** On PRs the comparison is path-filtered (scheduler / mail paths) and label-gated, not run on every PR — K trials x two configs x a GPU runner is expensive.

## Consequences

- Every scheduler perf claim gets the same reproducible basis; "is it flat?" stops being a per-change argument and becomes a verdict with a stated band.
- The paired-delta + same-runner design means a *modest* K resolves a real change even while the absolute numbers swing, which is what makes replication affordable.
- Follow-on build: the harness comparison mode (K-trial driver, paired stats, classification, JSON + table output) and a CI workflow + sticky-comment poster.
- K-trial replication is expensive — the reason for the path-filter + label trigger and the informational-not-gating posture.
- The method is honest about its floor: heavy-tailed run-to-run noise leaves genuinely-small effects below the band. Sub-microsecond dispatch-glue changes stay microbench territory (consistent with the prior finding that the latency sweep cannot resolve route-path-level deltas); this comparison is for changes that move a percentile beyond the run-to-run band.
- Reports are advisory: a real regression can still merge if a human ignores the comment. Accepted trade for not blocking legitimate PRs on noise.

## Alternatives considered

- **Single-run before/after comparison** — rejected: run-to-run noise (100-1000x the signal) makes single-run deltas meaningless. This is the status quo being replaced.
- **Cached baseline (run main periodically, compare PRs against stored numbers)** — rejected: the cached baseline is from a different runner and time, so cross-runner and thermal drift contaminate every comparison — precisely the noise the method exists to cancel. Same-runner interleaving is the whole point.
- **Two independent trial clouds (unpaired)** — rejected: leaves the shared runner drift in both samples, so the band on the difference is needlessly wide; pairing cancels it.
- **Mean +/- stddev** — rejected: the run-to-run distribution is heavy-tailed, so the mean is not robust; median + IQR is.
- **Within-run bootstrap only (no replication)** — rejected: captures sampling error but not the dominant between-run condition variance, so it reports tight bands that are wrong.
- **Hard merge gate on any regression** — rejected for now: false positives under noise erode trust faster than they catch real regressions. Informational first; soft gate only once the band is trusted.
