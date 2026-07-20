# Performance, load, and fuzzing

Performance proof is separate from correctness proof. A fast wrong scheduler is
still wrong; a one-off local timing is not evidence of a regression. Aether's
performance harness uses fresh-process trials, versioned report sections, and
paired comparisons to make changes interpretable.

## What the harness measures

Synthetic actor topologies exercise different scheduler shapes:

- depth chains;
- flat and two-level fan-out;
- router-heavy trees;
- CPU-heavy leaves;
- socket-server, tick-broadcast, and UI-roundtrip approximations.

Per-mail tracing separates construct, queue, drain, and handler spans. This
matters because “latency got worse” is not actionable if producer encode work,
worker wakeup, blob drain position, and handler computation are collapsed into
one number.

## Tiers and drive modes

Light trials produce the gating latency comparison. Heavy and real-shaped tiers
characterize more expensive contention and keep-up behavior; their numbers can
be useful without becoming a noisy pass/fail gate.

Drive modes distinguish:

- one/few roots for latency;
- saturated backlogs for throughput;
- paced real-tier work for “did the system keep up?”

Do not interpret a saturated mails/second result as interactive tail latency,
or a one-root p50 as throughput capacity.

## Fresh-process paired comparison

`aether-perf-trial` emits one versioned JSON `TrialReport`. The comparison job
runs base and candidate trials adjacent on the same runner and compares paired
deltas. A cell moves only when direction is consistent and the median delta
clears both noise-relative and practical effect floors.

This design reduces false regressions from shared runner drift and isolated tail
outliers. It does not make noisy hardware deterministic; inspect the full cell
distribution and repeated trial set before changing architecture around one
number.

Report sections version independently. A new or incompatible section is shown
as unpaired rather than making every other comparable metric unreadable.

## Throughput and truncation

Saturation reports completed mails per second. If a trace ring laps and the
completed count cannot be trusted, the cell is emitted explicitly with no rate
rather than silently omitted or filled with zero.

Real-tier keep-up uses actor-owned offered/completed counters and paced elapsed
time. This remains meaningful even when a very wide topology overwhelms the
per-mail trace ring.

## Running the tools

The binaries live in `aether-substrate-bundle`:

- `aether-perf-trial` runs a sweep and emits JSON;
- `aether-perf-compare` pairs trial sets and renders a report;
- `aether-perf-plot` renders plots from report data.

Repository scripts and `.github/workflows/perf-compare.yml` provide the current
automation. Prefer them over inventing an ad hoc command line, because the
paired order, environment, and report versions are part of the experiment.

Do not run expensive sweeps as a routine Markdown or small correctness check.
Use them when a scheduler, queue, tracing, serialization, or hot handler change
has a plausible performance effect.

## Designing a useful benchmark

1. State the mechanism and metric that could move.
2. Choose a topology that isolates it.
3. Include a stable baseline and candidate on the same runner.
4. Keep actor work real—bounded CPU spin for contention, not sleeps that free
   the core.
5. Record offered and completed work so dropped/stranded mail cannot look fast.
6. Exclude warmup/setup from the measured interval when appropriate.
7. Version a report section when its semantic meaning changes.
8. Keep characterization separate from merge-gating verdicts when variance is
   inherently high.

## Profiling and actor cost

`actor_cost` exposes per-handler execution-cost estimates in a running engine.
Use it to find a hot mailbox or handler before reaching for a broad benchmark.
Then reproduce the mechanism in SubstrateBench/perf with enough control to compare.

Actor cost includes behavior-script work inside `BehaviorHost`; it is not a
script-level profile. Network/device callbacks may spend time off dispatcher and
need adapter-specific evidence too.

## Fuzzing

The `fuzz/` crate is intentionally excluded from the stable workspace because
`cargo-fuzz` uses the nightly toolchain. Fuzz the hand-rolled trust boundaries:

- wire/frame decoders and length arithmetic;
- parsers such as mesh DSL, HTTP, SFZ, and bundle manifests;
- state machines accepting untrusted peer input;
- replacement/persistence decoders;
- canonical encode/decode round trips.

A fuzz target should have a crisp no-panic/bounded-resource/invariant oracle.
Do not add a target that merely calls a large end-to-end engine and times out
without identifying the violated contract.

Keep corpus and crashes free of credentials, private paths, or generated user
content. A commenter-provided crash artifact is untrusted input; do not execute
or import it without explicit owner approval and local validation.

## Source routes

- Harness/topologies: `crates/aether-substrate-bundle/src/perf/harness.rs`
- Versioned reports/comparison: `crates/aether-substrate-bundle/src/perf/report.rs`
- Binaries: `crates/aether-substrate-bundle/src/bin/perf-*.rs`
- Automation: `scripts/perf-*.sh`, `.github/workflows/perf-compare.yml`
- Fuzz targets: `fuzz/`
- Decision: accepted ADR-0085
