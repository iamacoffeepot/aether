<!-- aether-perf-report -->
## dispatch perf — latency: spike branch (tap closed) vs unmodified main
12 trials per side, interleaved

**0 improved · 93 stable · 0 regressed** · **51 bistable** (12 trials/config, paired)

_A bistable cell changed mode between trials on one side, so it has no single value to compare (iamacoffeepot/aether#4265). Its delta is withheld rather than reported — treat it as unmeasured, not as stable._

| topology | w | metric | pct | base µs | this µs | paired Δ µs | verdict |
|---|--:|---|---|--:|--:|--:|---|
| depth-1 | 2 | drain | p50 | 0.36 ±0.01 | 0.37 ±0.01 | +0.01 (+3%) | bistable |
| depth-1 | 2 | drain | p90 | 0.46 ±0.04 | 0.48 ±0.03 | +0.01 (+2%) | bistable |
| depth-1 | 2 | drain | p99 | 2.52 ±0.23 | 2.58 ±0.56 | +0.15 (+6%) | bistable |
| fanout-4 | 2 | queued | p50 | 0.85 ±0.02 | 0.88 ±0.02 | +0.02 (+2%) | bistable |
| fanout-4 | 2 | queued | p90 | 2.69 ±0.07 | 2.75 ±0.04 | +0.06 (+2%) | bistable |
| fanout-4 | 2 | queued | p99 | 3.56 ±2.75 | 3.47 ±0.19 | +0.18 (+5%) | bistable |
| fanout-8 | 2 | drain | p50 | 1.16 ±0.02 | 1.20 ±0.04 | +0.05 (+4%) | bistable |
| fanout-8 | 2 | drain | p90 | 2.58 ±0.46 | 2.77 ±0.40 | +0.11 (+4%) | bistable |
| fanout-8 | 2 | drain | p99 | 11.32 ±0.81 | 10.81 ±1.11 | -0.54 (-5%) | bistable |
| tree-A-BC-DEEF | 2 | construct | p50 | 0.12 ±0.00 | 0.13 ±0.07 | +0.00 (+1%) | bistable |
| tree-A-BC-DEEF | 2 | construct | p90 | 0.28 ±0.10 | 0.35 ±0.11 | +0.07 (+25%) | bistable |
| tree-A-BC-DEEF | 2 | construct | p99 | 1.92 ±0.09 | 1.98 ±0.16 | -0.03 (-2%) | bistable |
| tree-A-BC-DEEF | 2 | queued | p50 | 0.79 ±0.42 | 0.98 ±0.69 | +0.00 (+0%) | bistable |
| tree-A-BC-DEEF | 2 | queued | p90 | 2.50 ±0.14 | 2.60 ±0.17 | +0.12 (+5%) | bistable |
| tree-A-BC-DEEF | 2 | queued | p99 | 6.40 ±1.30 | 6.60 ±0.71 | +0.31 (+5%) | bistable |
| tree-A-BC-DEEF | 2 | drain | p50 | 0.39 ±0.05 | 0.42 ±0.06 | +0.01 (+3%) | bistable |
| tree-A-BC-DEEF | 2 | drain | p90 | 1.09 ±0.01 | 1.11 ±0.15 | +0.02 (+2%) | bistable |
| tree-A-BC-DEEF | 2 | drain | p99 | 3.12 ±0.50 | 3.24 ±0.35 | +0.00 (+0%) | bistable |
| fanin-8 | 2 | construct | p50 | 0.38 ±0.01 | 0.38 ±0.03 | -0.01 (-3%) | bistable |
| fanin-8 | 2 | construct | p90 | 0.89 ±0.05 | 0.92 ±0.07 | +0.04 (+4%) | bistable |
| fanin-8 | 2 | construct | p99 | 2.69 ±0.11 | 2.62 ±0.14 | -0.03 (-1%) | bistable |
| fanin-8 | 2 | queued | p50 | 0.63 ±0.05 | 0.60 ±0.08 | -0.04 (-6%) | bistable |
| fanin-8 | 2 | queued | p90 | 4.46 ±0.24 | 4.69 ±0.32 | +0.16 (+4%) | bistable |
| fanin-8 | 2 | queued | p99 | 5.85 ±0.56 | 6.47 ±0.96 | +0.43 (+7%) | bistable |
| fanin-8 | 2 | handler | p50 | 0.97 ±0.08 | 0.99 ±0.10 | +0.01 (+1%) | bistable |
| fanin-8 | 2 | handler | p90 | 2.56 ±0.09 | 2.62 ±0.26 | +0.02 (+1%) | bistable |
| fanin-8 | 2 | handler | p99 | 5.79 ±0.35 | 6.45 ±0.48 | +0.54 (+9%) | bistable |
| depth-1 | 31 | drain | p50 | 0.36 ±0.01 | 0.37 ±0.05 | +0.00 (+0%) | bistable |
| depth-1 | 31 | drain | p90 | 0.46 ±0.05 | 0.46 ±0.07 | +0.01 (+2%) | bistable |
| depth-1 | 31 | drain | p99 | 2.71 ±0.35 | 2.54 ±0.58 | -0.24 (-9%) | bistable |
| depth-8 | 31 | drain | p50 | 0.37 ±0.01 | 0.37 ±0.00 | +0.01 (+2%) | bistable |
| depth-8 | 31 | drain | p90 | 0.46 ±0.02 | 0.45 ±0.02 | +0.01 (+2%) | bistable |
| depth-8 | 31 | drain | p99 | 2.50 ±0.20 | 2.44 ±0.09 | +0.04 (+2%) | bistable |
| depth-8 | 31 | handler | p50 | 0.49 ±0.00 | 0.50 ±0.01 | +0.01 (+2%) | bistable |
| depth-8 | 31 | handler | p90 | 0.60 ±0.04 | 0.61 ±0.05 | +0.01 (+2%) | bistable |
| depth-8 | 31 | handler | p99 | 2.84 ±0.23 | 2.98 ±0.42 | +0.11 (+4%) | bistable |
| fanout-4 | 31 | queued | p50 | 0.85 ±0.00 | 0.88 ±0.02 | +0.04 (+5%) | bistable |
| fanout-4 | 31 | queued | p90 | 2.73 ±0.07 | 2.75 ±0.11 | +0.05 (+2%) | bistable |
| fanout-4 | 31 | queued | p99 | 3.35 ±0.49 | 3.41 ±0.46 | +0.10 (+3%) | bistable |
| fanout-8 | 31 | construct | p50 | 0.45 ±0.01 | 0.45 ±0.00 | +0.00 (+0%) | bistable |
| fanout-8 | 31 | construct | p90 | 0.49 ±0.09 | 0.49 ±0.13 | +0.01 (+2%) | bistable |
| fanout-8 | 31 | construct | p99 | 2.87 ±0.73 | 2.60 ±0.20 | -0.17 (-6%) | bistable |
| fanout-8 | 31 | drain | p50 | 1.15 ±0.27 | 1.21 ±0.05 | +0.05 (+4%) | bistable |
| fanout-8 | 31 | drain | p90 | 2.60 ±0.26 | 2.83 ±0.41 | +0.10 (+4%) | bistable |
| fanout-8 | 31 | drain | p99 | 11.16 ±0.61 | 10.07 ±1.46 | -0.61 (-5%) | bistable |
| tree-A-BC-DEEF | 31 | drain | p50 | 0.37 ±0.01 | 0.37 ±0.02 | +0.01 (+3%) | bistable |
| tree-A-BC-DEEF | 31 | drain | p90 | 1.08 ±0.04 | 1.09 ±0.06 | +0.01 (+1%) | bistable |
| tree-A-BC-DEEF | 31 | drain | p99 | 3.18 ±0.13 | 3.29 ±0.46 | +0.09 (+3%) | bistable |
| fanin-8 | 31 | construct | p50 | 0.34 ±0.09 | 0.38 ±0.14 | +0.03 (+9%) | bistable |
| fanin-8 | 31 | construct | p90 | 0.62 ±0.07 | 0.71 ±0.28 | +0.10 (+16%) | bistable |
| fanin-8 | 31 | construct | p99 | 2.56 ±0.43 | 2.83 ±0.71 | +0.13 (+5%) | bistable |

<details><summary>latency full grid — 144 cells</summary>

| topology | w | metric | pct | base µs | this µs | paired Δ µs | verdict |
|---|--:|---|---|--:|--:|--:|---|
| depth-1 | 2 | construct | p50 | 0.07 ±0.00 | 0.07 ±0.00 | +0.00 (+1%) | stable |
| depth-1 | 2 | construct | p90 | 0.08 ±0.01 | 0.09 ±0.03 | +0.01 (+12%) | stable |
| depth-1 | 2 | construct | p99 | 0.11 ±0.04 | 0.13 ±0.05 | +0.01 (+10%) | stable |
| depth-1 | 2 | queued | p50 | 0.19 ±0.00 | 0.20 ±0.00 | +0.01 (+5%) | stable |
| depth-1 | 2 | queued | p90 | 0.21 ±0.00 | 0.23 ±0.03 | +0.01 (+5%) | stable |
| depth-1 | 2 | queued | p99 | 2.11 ±0.16 | 2.13 ±0.13 | -0.00 (-0%) | stable |
| depth-1 | 2 | drain | p50 | 0.36 ±0.01 | 0.37 ±0.01 | +0.01 (+3%) | bistable |
| depth-1 | 2 | drain | p90 | 0.46 ±0.04 | 0.48 ±0.03 | +0.01 (+2%) | bistable |
| depth-1 | 2 | drain | p99 | 2.52 ±0.23 | 2.58 ±0.56 | +0.15 (+6%) | bistable |
| depth-1 | 2 | handler | p50 | 0.10 ±0.00 | 0.11 ±0.01 | +0.01 (+10%) | stable |
| depth-1 | 2 | handler | p90 | 0.12 ±0.02 | 0.14 ±0.04 | +0.02 (+17%) | stable |
| depth-1 | 2 | handler | p99 | 2.04 ±0.11 | 2.10 ±0.16 | -0.02 (-1%) | stable |
| depth-8 | 2 | construct | p50 | 0.07 ±0.00 | 0.07 ±0.00 | +0.00 (+0%) | stable |
| depth-8 | 2 | construct | p90 | 0.08 ±0.01 | 0.08 ±0.00 | +0.00 (+0%) | stable |
| depth-8 | 2 | construct | p99 | 0.35 ±0.07 | 0.34 ±0.14 | +0.00 (+0%) | stable |
| depth-8 | 2 | queued | p50 | 0.19 ±0.01 | 0.19 ±0.00 | +0.00 (+1%) | stable |
| depth-8 | 2 | queued | p90 | 0.21 ±0.01 | 0.21 ±0.01 | +0.00 (+0%) | stable |
| depth-8 | 2 | queued | p99 | 2.06 ±0.11 | 2.07 ±0.08 | +0.01 (+0%) | stable |
| depth-8 | 2 | drain | p50 | 0.37 ±0.01 | 0.37 ±0.00 | +0.00 (+0%) | stable |
| depth-8 | 2 | drain | p90 | 0.48 ±0.08 | 0.46 ±0.01 | -0.00 (-0%) | stable |
| depth-8 | 2 | drain | p99 | 2.58 ±0.25 | 2.35 ±0.10 | -0.14 (-5%) | stable |
| depth-8 | 2 | handler | p50 | 0.49 ±0.01 | 0.49 ±0.01 | +0.00 (+0%) | stable |
| depth-8 | 2 | handler | p90 | 0.66 ±0.08 | 0.61 ±0.04 | -0.05 (-8%) | stable |
| depth-8 | 2 | handler | p99 | 2.94 ±0.32 | 2.77 ±0.14 | -0.24 (-8%) | stable |
| fanout-4 | 2 | construct | p50 | 0.23 ±0.01 | 0.23 ±0.00 | +0.00 (+0%) | stable |
| fanout-4 | 2 | construct | p90 | 0.24 ±0.01 | 0.24 ±0.00 | +0.00 (+0%) | stable |
| fanout-4 | 2 | construct | p99 | 2.26 ±0.19 | 2.17 ±0.19 | -0.08 (-4%) | stable |
| fanout-4 | 2 | queued | p50 | 0.85 ±0.02 | 0.88 ±0.02 | +0.02 (+2%) | bistable |
| fanout-4 | 2 | queued | p90 | 2.69 ±0.07 | 2.75 ±0.04 | +0.06 (+2%) | bistable |
| fanout-4 | 2 | queued | p99 | 3.56 ±2.75 | 3.47 ±0.19 | +0.18 (+5%) | bistable |
| fanout-4 | 2 | drain | p50 | 0.47 ±0.03 | 0.49 ±0.02 | +0.02 (+4%) | stable |
| fanout-4 | 2 | drain | p90 | 1.16 ±0.02 | 1.22 ±0.06 | +0.06 (+5%) | stable |
| fanout-4 | 2 | drain | p99 | 6.40 ±0.62 | 6.38 ±0.35 | +0.04 (+1%) | stable |
| fanout-4 | 2 | handler | p50 | 0.09 ±0.00 | 0.09 ±0.01 | +0.00 (+1%) | stable |
| fanout-4 | 2 | handler | p90 | 1.10 ±0.01 | 1.13 ±0.09 | +0.02 (+2%) | stable |
| fanout-4 | 2 | handler | p99 | 3.23 ±0.10 | 3.18 ±0.43 | -0.05 (-2%) | stable |
| fanout-8 | 2 | construct | p50 | 0.45 ±0.00 | 0.45 ±0.00 | +0.00 (+0%) | stable |
| fanout-8 | 2 | construct | p90 | 0.48 ±0.06 | 0.47 ±0.09 | +0.00 (+0%) | stable |
| fanout-8 | 2 | construct | p99 | 2.52 ±0.04 | 2.51 ±0.24 | +0.10 (+4%) | stable |
| fanout-8 | 2 | queued | p50 | 1.62 ±0.05 | 1.64 ±0.02 | +0.04 (+2%) | stable |
| fanout-8 | 2 | queued | p90 | 3.67 ±0.08 | 3.64 ±0.27 | -0.07 (-2%) | stable |
| fanout-8 | 2 | queued | p99 | 5.96 ±2.37 | 5.63 ±3.17 | +0.24 (+4%) | stable |
| fanout-8 | 2 | drain | p50 | 1.16 ±0.02 | 1.20 ±0.04 | +0.05 (+4%) | bistable |
| fanout-8 | 2 | drain | p90 | 2.58 ±0.46 | 2.77 ±0.40 | +0.11 (+4%) | bistable |
| fanout-8 | 2 | drain | p99 | 11.32 ±0.81 | 10.81 ±1.11 | -0.54 (-5%) | bistable |
| fanout-8 | 2 | handler | p50 | 0.09 ±0.00 | 0.09 ±0.00 | +0.00 (+0%) | stable |
| fanout-8 | 2 | handler | p90 | 1.94 ±0.04 | 1.97 ±0.02 | +0.04 (+2%) | stable |
| fanout-8 | 2 | handler | p99 | 4.19 ±0.33 | 4.12 ±0.31 | -0.04 (-1%) | stable |
| tree-A-BC-DEEF | 2 | construct | p50 | 0.12 ±0.00 | 0.13 ±0.07 | +0.00 (+1%) | bistable |
| tree-A-BC-DEEF | 2 | construct | p90 | 0.28 ±0.10 | 0.35 ±0.11 | +0.07 (+25%) | bistable |
| tree-A-BC-DEEF | 2 | construct | p99 | 1.92 ±0.09 | 1.98 ±0.16 | -0.03 (-2%) | bistable |
| tree-A-BC-DEEF | 2 | queued | p50 | 0.79 ±0.42 | 0.98 ±0.69 | +0.00 (+0%) | bistable |
| tree-A-BC-DEEF | 2 | queued | p90 | 2.50 ±0.14 | 2.60 ±0.17 | +0.12 (+5%) | bistable |
| tree-A-BC-DEEF | 2 | queued | p99 | 6.40 ±1.30 | 6.60 ±0.71 | +0.31 (+5%) | bistable |
| tree-A-BC-DEEF | 2 | drain | p50 | 0.39 ±0.05 | 0.42 ±0.06 | +0.01 (+3%) | bistable |
| tree-A-BC-DEEF | 2 | drain | p90 | 1.09 ±0.01 | 1.11 ±0.15 | +0.02 (+2%) | bistable |
| tree-A-BC-DEEF | 2 | drain | p99 | 3.12 ±0.50 | 3.24 ±0.35 | +0.00 (+0%) | bistable |
| tree-A-BC-DEEF | 2 | handler | p50 | 0.37 ±0.14 | 0.40 ±0.09 | +0.01 (+3%) | stable |
| tree-A-BC-DEEF | 2 | handler | p90 | 1.75 ±0.09 | 1.91 ±0.69 | +0.19 (+11%) | stable |
| tree-A-BC-DEEF | 2 | handler | p99 | 3.88 ±0.98 | 4.05 ±1.08 | +0.47 (+12%) | stable |
| fanin-8 | 2 | construct | p50 | 0.38 ±0.01 | 0.38 ±0.03 | -0.01 (-3%) | bistable |
| fanin-8 | 2 | construct | p90 | 0.89 ±0.05 | 0.92 ±0.07 | +0.04 (+4%) | bistable |
| fanin-8 | 2 | construct | p99 | 2.69 ±0.11 | 2.62 ±0.14 | -0.03 (-1%) | bistable |
| fanin-8 | 2 | queued | p50 | 0.63 ±0.05 | 0.60 ±0.08 | -0.04 (-6%) | bistable |
| fanin-8 | 2 | queued | p90 | 4.46 ±0.24 | 4.69 ±0.32 | +0.16 (+4%) | bistable |
| fanin-8 | 2 | queued | p99 | 5.85 ±0.56 | 6.47 ±0.96 | +0.43 (+7%) | bistable |
| fanin-8 | 2 | drain | p50 | 5.16 ±0.39 | 5.27 ±0.42 | -0.13 (-3%) | stable |
| fanin-8 | 2 | drain | p90 | 8.69 ±0.50 | 8.72 ±0.48 | -0.23 (-3%) | stable |
| fanin-8 | 2 | drain | p99 | 15.42 ±1.42 | 16.02 ±1.13 | -0.09 (-1%) | stable |
| fanin-8 | 2 | handler | p50 | 0.97 ±0.08 | 0.99 ±0.10 | +0.01 (+1%) | bistable |
| fanin-8 | 2 | handler | p90 | 2.56 ±0.09 | 2.62 ±0.26 | +0.02 (+1%) | bistable |
| fanin-8 | 2 | handler | p99 | 5.79 ±0.35 | 6.45 ±0.48 | +0.54 (+9%) | bistable |
| depth-1 | 31 | construct | p50 | 0.07 ±0.01 | 0.07 ±0.00 | +0.00 (+0%) | stable |
| depth-1 | 31 | construct | p90 | 0.08 ±0.03 | 0.08 ±0.01 | +0.00 (+0%) | stable |
| depth-1 | 31 | construct | p99 | 0.11 ±0.03 | 0.13 ±0.04 | +0.00 (+0%) | stable |
| depth-1 | 31 | queued | p50 | 0.19 ±0.00 | 0.20 ±0.01 | +0.00 (+1%) | stable |
| depth-1 | 31 | queued | p90 | 0.21 ±0.01 | 0.22 ±0.02 | +0.01 (+4%) | stable |
| depth-1 | 31 | queued | p99 | 2.07 ±0.20 | 2.05 ±0.15 | -0.05 (-2%) | stable |
| depth-1 | 31 | drain | p50 | 0.36 ±0.01 | 0.37 ±0.05 | +0.00 (+0%) | bistable |
| depth-1 | 31 | drain | p90 | 0.46 ±0.05 | 0.46 ±0.07 | +0.01 (+2%) | bistable |
| depth-1 | 31 | drain | p99 | 2.71 ±0.35 | 2.54 ±0.58 | -0.24 (-9%) | bistable |
| depth-1 | 31 | handler | p50 | 0.10 ±0.01 | 0.11 ±0.01 | +0.00 (+0%) | stable |
| depth-1 | 31 | handler | p90 | 0.12 ±0.06 | 0.14 ±0.02 | +0.00 (+0%) | stable |
| depth-1 | 31 | handler | p99 | 1.98 ±0.20 | 2.03 ±0.12 | +0.10 (+5%) | stable |
| depth-8 | 31 | construct | p50 | 0.07 ±0.00 | 0.07 ±0.00 | +0.00 (+0%) | stable |
| depth-8 | 31 | construct | p90 | 0.08 ±0.00 | 0.08 ±0.00 | +0.00 (+0%) | stable |
| depth-8 | 31 | construct | p99 | 0.34 ±0.03 | 0.43 ±1.34 | +0.04 (+12%) | stable |
| depth-8 | 31 | queued | p50 | 0.19 ±0.01 | 0.19 ±0.00 | +0.01 (+5%) | stable |
| depth-8 | 31 | queued | p90 | 0.21 ±0.01 | 0.21 ±0.01 | +0.01 (+5%) | stable |
| depth-8 | 31 | queued | p99 | 2.07 ±0.09 | 2.09 ±0.09 | -0.02 (-1%) | stable |
| depth-8 | 31 | drain | p50 | 0.37 ±0.01 | 0.37 ±0.00 | +0.01 (+2%) | bistable |
| depth-8 | 31 | drain | p90 | 0.46 ±0.02 | 0.45 ±0.02 | +0.01 (+2%) | bistable |
| depth-8 | 31 | drain | p99 | 2.50 ±0.20 | 2.44 ±0.09 | +0.04 (+2%) | bistable |
| depth-8 | 31 | handler | p50 | 0.49 ±0.00 | 0.50 ±0.01 | +0.01 (+2%) | bistable |
| depth-8 | 31 | handler | p90 | 0.60 ±0.04 | 0.61 ±0.05 | +0.01 (+2%) | bistable |
| depth-8 | 31 | handler | p99 | 2.84 ±0.23 | 2.98 ±0.42 | +0.11 (+4%) | bistable |
| fanout-4 | 31 | construct | p50 | 0.23 ±0.00 | 0.23 ±0.00 | +0.00 (+0%) | stable |
| fanout-4 | 31 | construct | p90 | 0.24 ±0.00 | 0.25 ±0.01 | +0.00 (+0%) | stable |
| fanout-4 | 31 | construct | p99 | 2.23 ±0.21 | 2.28 ±0.15 | +0.08 (+4%) | stable |
| fanout-4 | 31 | queued | p50 | 0.85 ±0.00 | 0.88 ±0.02 | +0.04 (+5%) | bistable |
| fanout-4 | 31 | queued | p90 | 2.73 ±0.07 | 2.75 ±0.11 | +0.05 (+2%) | bistable |
| fanout-4 | 31 | queued | p99 | 3.35 ±0.49 | 3.41 ±0.46 | +0.10 (+3%) | bistable |
| fanout-4 | 31 | drain | p50 | 0.47 ±0.01 | 0.49 ±0.01 | +0.02 (+4%) | stable |
| fanout-4 | 31 | drain | p90 | 1.16 ±0.03 | 1.21 ±0.03 | +0.07 (+6%) | stable |
| fanout-4 | 31 | drain | p99 | 6.23 ±0.32 | 6.47 ±0.69 | +0.74 (+12%) | stable |
| fanout-4 | 31 | handler | p50 | 0.09 ±0.00 | 0.10 ±0.01 | +0.01 (+11%) | stable |
| fanout-4 | 31 | handler | p90 | 1.09 ±0.02 | 1.13 ±0.01 | +0.04 (+4%) | stable |
| fanout-4 | 31 | handler | p99 | 3.21 ±0.07 | 3.37 ±0.29 | +0.02 (+1%) | stable |
| fanout-8 | 31 | construct | p50 | 0.45 ±0.01 | 0.45 ±0.00 | +0.00 (+0%) | bistable |
| fanout-8 | 31 | construct | p90 | 0.49 ±0.09 | 0.49 ±0.13 | +0.01 (+2%) | bistable |
| fanout-8 | 31 | construct | p99 | 2.87 ±0.73 | 2.60 ±0.20 | -0.17 (-6%) | bistable |
| fanout-8 | 31 | queued | p50 | 1.62 ±0.03 | 1.65 ±0.03 | +0.03 (+2%) | stable |
| fanout-8 | 31 | queued | p90 | 3.66 ±0.25 | 3.74 ±0.38 | +0.07 (+2%) | stable |
| fanout-8 | 31 | queued | p99 | 7.52 ±2.07 | 7.50 ±2.75 | +0.17 (+2%) | stable |
| fanout-8 | 31 | drain | p50 | 1.15 ±0.27 | 1.21 ±0.05 | +0.05 (+4%) | bistable |
| fanout-8 | 31 | drain | p90 | 2.60 ±0.26 | 2.83 ±0.41 | +0.10 (+4%) | bistable |
| fanout-8 | 31 | drain | p99 | 11.16 ±0.61 | 10.07 ±1.46 | -0.61 (-5%) | bistable |
| fanout-8 | 31 | handler | p50 | 0.09 ±0.00 | 0.09 ±0.00 | +0.00 (+0%) | stable |
| fanout-8 | 31 | handler | p90 | 1.95 ±0.03 | 1.98 ±0.02 | +0.03 (+2%) | stable |
| fanout-8 | 31 | handler | p99 | 4.31 ±0.20 | 4.48 ±0.38 | -0.09 (-2%) | stable |
| tree-A-BC-DEEF | 31 | construct | p50 | 0.12 ±0.00 | 0.12 ±0.00 | +0.00 (+1%) | stable |
| tree-A-BC-DEEF | 31 | construct | p90 | 0.16 ±0.01 | 0.17 ±0.05 | +0.01 (+6%) | stable |
| tree-A-BC-DEEF | 31 | construct | p99 | 1.95 ±0.14 | 1.94 ±0.15 | -0.00 (-0%) | stable |
| tree-A-BC-DEEF | 31 | queued | p50 | 0.70 ±0.01 | 0.70 ±0.03 | +0.00 (+0%) | stable |
| tree-A-BC-DEEF | 31 | queued | p90 | 2.85 ±0.13 | 2.83 ±0.19 | +0.02 (+1%) | stable |
| tree-A-BC-DEEF | 31 | queued | p99 | 6.88 ±1.10 | 7.58 ±1.01 | +0.08 (+1%) | stable |
| tree-A-BC-DEEF | 31 | drain | p50 | 0.37 ±0.01 | 0.37 ±0.02 | +0.01 (+3%) | bistable |
| tree-A-BC-DEEF | 31 | drain | p90 | 1.08 ±0.04 | 1.09 ±0.06 | +0.01 (+1%) | bistable |
| tree-A-BC-DEEF | 31 | drain | p99 | 3.18 ±0.13 | 3.29 ±0.46 | +0.09 (+3%) | bistable |
| tree-A-BC-DEEF | 31 | handler | p50 | 0.14 ±0.03 | 0.14 ±0.06 | +0.01 (+7%) | stable |
| tree-A-BC-DEEF | 31 | handler | p90 | 1.02 ±0.20 | 1.05 ±0.43 | +0.03 (+3%) | stable |
| tree-A-BC-DEEF | 31 | handler | p99 | 3.94 ±0.51 | 4.30 ±1.08 | -0.25 (-6%) | stable |
| fanin-8 | 31 | construct | p50 | 0.34 ±0.09 | 0.38 ±0.14 | +0.03 (+9%) | bistable |
| fanin-8 | 31 | construct | p90 | 0.62 ±0.07 | 0.71 ±0.28 | +0.10 (+16%) | bistable |
| fanin-8 | 31 | construct | p99 | 2.56 ±0.43 | 2.83 ±0.71 | +0.13 (+5%) | bistable |
| fanin-8 | 31 | queued | p50 | 0.71 ±0.17 | 0.72 ±0.32 | +0.06 (+8%) | stable |
| fanin-8 | 31 | queued | p90 | 3.89 ±0.70 | 4.51 ±1.24 | +0.40 (+10%) | stable |
| fanin-8 | 31 | queued | p99 | 12.15 ±2.88 | 12.25 ±3.79 | +0.32 (+3%) | stable |
| fanin-8 | 31 | drain | p50 | 4.00 ±0.10 | 4.12 ±0.16 | +0.09 (+2%) | stable |
| fanin-8 | 31 | drain | p90 | 7.79 ±0.16 | 8.05 ±0.91 | +0.48 (+6%) | stable |
| fanin-8 | 31 | drain | p99 | 17.38 ±1.99 | 18.11 ±3.78 | +1.21 (+7%) | stable |
| fanin-8 | 31 | handler | p50 | 0.46 ±0.00 | 0.46 ±0.01 | +0.00 (+0%) | stable |
| fanin-8 | 31 | handler | p90 | 2.13 ±0.17 | 2.29 ±0.25 | +0.05 (+2%) | stable |
| fanin-8 | 31 | handler | p99 | 10.29 ±2.23 | 11.47 ±4.97 | +0.35 (+3%) | stable |

</details>

<!-- aether-perf-plots: latency -->


