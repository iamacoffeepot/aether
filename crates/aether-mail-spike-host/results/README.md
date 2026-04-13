# Mail-spike benchmark results

CSV output from running `cargo run --release -p aether-mail-spike-host` lands here. Files are gitignored — they vary per machine and across runs, and we don't want to commit benchmark output.

Always use `--release`. Both the host binary and the wasm guest pick up the host's profile (release host → release guest, debug host → debug guest), so a debug run gives you debug-host numbers measuring debug-guest work — meaningless for benchmarking.

The numbers that *do* land in history go through ADR-0003, which records the verdict on issue #7's success criteria and points at the specific recommended next step.

## CSV columns

Each workload writes its own CSV with its own dimension columns. Common columns:

`workload, <dim_a>, <dim_b>, iterations, total_ms, frames_per_sec, mean_us, p50_us, p95_us, p99_us`

- `workload` — name of the workload (`broadcast`, `bulk`, `chain`, `mixed`).
- `iterations` — how many full frames ran inside the per-cell time budget.
- `frames_per_sec` — derived throughput.
- `mean_us` / `p50_us` / `p95_us` / `p99_us` — per-frame latency distribution in microseconds.

Per-workload dimensions:

| Workload | dim_a | dim_b | What it sweeps |
| --- | --- | --- | --- |
| `broadcast` | `n_actors` | `work_per_actor` | Fanout × per-actor work (sequential; ADR-0003) |
| `bulk` | `batch_size` | `work_per_item` | One sender → one receiver, varying batch size; tests batching amortization |
| `chain` | `depth` | `work_per_link` | Sequential dispatch through D actors |
| `mixed` | `n_actors` | `work_per_actor` | Broadcast tick + neighbor phase, 2N mails per frame (sequential) |
| `parallel_broadcast` | `n_actors` | `k_workers` | Concurrent fanout across K worker threads (issue #14) |
| `parallel_mixed` | `n_actors` | `k_workers` | Two barriered phases per frame (heavy tick + light neighbor) |
| `churn` | `n_actors` | `k_workers` | Tiny work per tick — isolates scheduler dispatch floor |

Sequential CSVs come from `cargo run --release -p aether-mail-spike-host`.
Concurrent CSVs come from `cargo run --release -p aether-mail-spike-host --bin concurrent`.

## How to plot

`plot.py` reads the four CSVs and writes corresponding PNGs alongside (also gitignored). It's a uv script with PEP 723 inline dependencies (matplotlib, numpy), so:

```sh
cd crates/aether-mail-spike-host/results
uv run plot.py
```

uv resolves and caches the deps in an isolated environment on first run; subsequent runs are instant. Outputs:

- `broadcast_mean.png`, `broadcast_p99.png` — heatmaps over `n_actors × work_per_actor`. Cells exceeding the 16.67ms 60Hz budget get a red border.
- `mixed_mean.png`, `mixed_p99.png` — same shape as broadcast.
- `bulk.png` — two-panel: per-mail latency vs batch size (log-log) and amortized cost per item.
- `chain.png` — per-frame latency vs depth (mean and p99).
- `parallel_broadcast_{mean,p99}.png`, `parallel_mixed_{mean,p99}.png`, `churn_{mean,p99}.png` — heatmaps over `n_actors × k_workers` for each concurrent workload.
- `scheduler_speedup.png` — three subplots (one per concurrent workload) showing speedup vs K per N, with an ideal-linear reference line.
- `churn_dispatch_floor.png` — per-tick scheduler cost (mean_us / N, in ns) vs K for each N; rising curves are the signature of shared-queue contention.
