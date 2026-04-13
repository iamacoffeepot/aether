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
| `broadcast` | `n_actors` | `work_per_actor` | Fanout × per-actor work |
| `bulk` | `batch_size` | `work_per_item` | One sender → one receiver, varying batch size; tests batching amortization |
| `chain` | `depth` | `work_per_link` | Sequential dispatch through D actors |
| `mixed` | `n_actors` | `work_per_actor` | Broadcast tick + neighbor phase, 2N mails per frame |

## How to plot

Plotting script lands in a follow-up PR. Until then, point your favorite tool (gnuplot, pandas, etc.) at the CSV directly.
