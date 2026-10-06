<!-- aether-perf-report -->
## dispatch perf — throughput: spike branch (tap closed) vs unmodified main
12 trials per side, interleaved

**0 improved · 10 stable · 0 regressed** (12 trials/config, paired)

_throughput: no cells moved beyond the noise band._

<details><summary>throughput full grid — 10 cells</summary>

| topology | w | base k/s | this k/s | paired Δ k/s | verdict |
|---|--:|--:|--:|--:|---|
| depth-1 | 2 | 2400.5 ±175.0 | 2318.7 ±367.8 | -38.7 (-2%) | stable |
| depth-8 | 2 | 1785.6 ±347.4 | 1814.0 ±336.3 | -3.2 (-0%) | stable |
| fanout-4 | 2 | 1225.8 ±296.3 | 1351.3 ±153.3 | +48.5 (+4%) | stable |
| fanout-8 | 2 | 1401.8 ±429.1 | 1424.8 ±608.1 | +47.3 (+3%) | stable |
| tree-A-BC-DEEF | 2 | 1837.8 ±502.5 | 1950.0 ±407.8 | +25.5 (+1%) | stable |
| depth-1 | 31 | 2370.2 ±678.3 | 2218.5 ±635.4 | -57.5 (-2%) | stable |
| depth-8 | 31 | 2291.4 ±392.9 | 2596.3 ±664.2 | +156.0 (+7%) | stable |
| fanout-4 | 31 | 1155.7 ±99.6 | 1218.3 ±208.3 | +62.2 (+5%) | stable |
| fanout-8 | 31 | 1266.8 ±180.0 | 1256.1 ±219.9 | -6.3 (-0%) | stable |
| tree-A-BC-DEEF | 31 | 1831.3 ±591.4 | 1771.8 ±393.7 | +18.9 (+1%) | stable |

</details>


