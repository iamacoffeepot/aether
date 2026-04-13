# Mail-spike benchmark results

CSV output from running `cargo run --release -p aether-mail-spike-host` lands here. Files are gitignored — they vary per machine and across runs, and we don't want to commit benchmark output.

The numbers that *do* land in history go through ADR-0003, which records the verdict on issue #7's success criteria and points at the specific recommended next step.

## CSV columns

`workload, n_actors, work_per_actor, iterations, total_ms, frames_per_sec, mails_per_sec, mean_us, p50_us, p95_us, p99_us`

- `workload` — name of the workload (`broadcast`, later `bulk`, `chain`, `mixed`).
- `n_actors`, `work_per_actor` — the matrix cell.
- `iterations` — how many full frames ran inside the per-cell time budget.
- `frames_per_sec`, `mails_per_sec` — derived throughput.
- `mean_us` / `p50_us` / `p95_us` / `p99_us` — per-frame latency distribution in microseconds.

## How to plot

Plotting script lands in a follow-up PR. Until then, point your favorite tool (gnuplot, pandas, etc.) at the CSV directly.
