# Actor arena preallocation matrix

Each cell has 7 fresh-process samples. Cell order rotates by an evenly distributed stride and alternates forward/reverse between rounds. Cold rates include capacity reservation plus actor state initialization. Hot rates follow a warm/reset phase and contain bullet updates only.

Forced reserved-page touching: **disabled**. Allocation instrumentation: **disabled**.

## native forecast error

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | `native-forecast-hint-050` | native | 50.0% | 16 | 100% packed | live-bitmap | 16.14 | 3.02 | 14.63 | 10.33 | 10.33 | 1.158 | 0.053 | 65536 | 0.00 | 7.88 |
| 1 | `native-forecast-hint-075` | native | 75.0% | 16 | 100% packed | live-bitmap | 20.48 | 4.17 | 17.55 | 6.46 | 6.46 | 1.123 | 0.043 | 65536 | 0.00 | 7.86 |
| 2 | `native-forecast-hint-100` | native | 100.0% | 16 | 100% packed | live-bitmap | 17.59 | 4.21 | 14.13 | 0.00 | 0.00 | 1.132 | 0.036 | 65536 | 0.00 | 7.86 |
| 3 | `native-forecast-hint-125` | native | 125.0% | 16 | 100% packed | live-bitmap | 17.33 | 2.77 | 12.54 | 0.00 | 0.00 | 1.116 | 0.053 | 81920 | 1.00 | 8.97 |
| 4 | `native-forecast-hint-200` | native | 200.0% | 16 | 100% packed | live-bitmap | 19.69 | 3.91 | 12.89 | 0.00 | 0.00 | 1.132 | 0.055 | 131072 | 4.00 | 12.22 |
| 5 | `native-forecast-hint-400` | native | 400.0% | 16 | 100% packed | live-bitmap | 29.56 | 1.68 | 14.36 | 0.00 | 0.00 | 1.135 | 0.068 | 262144 | 12.00 | 20.97 |
## native growth chunk

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 6 | `native-chunk-hint-075-pages-01` | native | 75.0% | 1 | 100% packed | live-bitmap | 17.08 | 2.37 | 13.02 | 1.75 | 3.08 | 1.169 | 0.035 | 65536 | 0.00 | 8.05 |
| 7 | `native-chunk-hint-075-pages-04` | native | 75.0% | 4 | 100% packed | live-bitmap | 18.79 | 1.84 | 15.42 | 4.83 | 4.83 | 1.138 | 0.045 | 65536 | 0.00 | 7.94 |
| 8 | `native-chunk-hint-075-pages-16` | native | 75.0% | 16 | 100% packed | live-bitmap | 16.81 | 0.99 | 14.03 | 5.67 | 5.67 | 1.135 | 0.036 | 65536 | 0.00 | 7.86 |
| 9 | `native-chunk-hint-075-pages-64` | native | 75.0% | 64 | 100% packed | live-bitmap | 16.19 | 1.34 | 13.54 | 18.17 | 18.17 | 1.123 | 0.040 | 65536 | 0.00 | 7.89 |
| 10 | `native-chunk-hint-100-pages-01` | native | 100.0% | 1 | 100% packed | live-bitmap | 20.88 | 2.74 | 15.86 | 0.00 | 0.00 | 1.176 | 0.015 | 65536 | 0.00 | 8.05 |
| 11 | `native-chunk-hint-100-pages-04` | native | 100.0% | 4 | 100% packed | live-bitmap | 18.97 | 2.47 | 14.90 | 0.00 | 0.00 | 1.146 | 0.012 | 65536 | 0.00 | 7.92 |
| 12 | `native-chunk-hint-100-pages-16` | native | 100.0% | 16 | 100% packed | live-bitmap | 18.71 | 2.29 | 14.66 | 0.00 | 0.00 | 1.123 | 0.060 | 65536 | 0.00 | 7.86 |
| 13 | `native-chunk-hint-100-pages-64` | native | 100.0% | 64 | 100% packed | live-bitmap | 14.45 | 2.79 | 11.08 | 0.00 | 0.00 | 1.138 | 0.058 | 65536 | 0.00 | 7.89 |
| 14 | `native-chunk-hint-125-pages-01` | native | 125.0% | 1 | 100% packed | live-bitmap | 23.92 | 1.00 | 17.53 | 0.00 | 0.00 | 1.159 | 0.043 | 81920 | 1.00 | 9.20 |
| 15 | `native-chunk-hint-125-pages-04` | native | 125.0% | 4 | 100% packed | live-bitmap | 23.12 | 3.26 | 17.89 | 0.00 | 0.00 | 1.159 | 0.034 | 81920 | 1.00 | 9.09 |
| 16 | `native-chunk-hint-125-pages-16` | native | 125.0% | 16 | 100% packed | live-bitmap | 19.71 | 2.04 | 15.17 | 0.00 | 0.00 | 1.132 | 0.033 | 81920 | 1.00 | 8.97 |
| 17 | `native-chunk-hint-125-pages-64` | native | 125.0% | 64 | 100% packed | live-bitmap | 19.03 | 2.00 | 14.48 | 0.00 | 0.00 | 1.124 | 0.045 | 81920 | 1.00 | 8.97 |
## native sparse occupancy

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 18 | `native-sparse-live-025-packed-live-bitmap` | native | 200.0% | 16 | 25% packed | live-bitmap | 19.69 | 1.54 | 12.61 | 0.00 | 0.00 | 1.106 | 0.086 | 131072 | 7.00 | 12.53 |
| 19 | `native-sparse-live-025-packed-capacity-scan` | native | 200.0% | 16 | 25% packed | capacity-scan | 21.35 | 2.13 | 14.42 | 0.00 | 0.00 | 1.652 | 0.031 | 131072 | 7.00 | 12.67 |
| 20 | `native-sparse-live-025-random-live-bitmap` | native | 200.0% | 16 | 25% random | live-bitmap | 21.86 | 2.06 | 14.43 | 0.00 | 0.00 | 1.925 | 0.208 | 131072 | 7.00 | 12.72 |
| 21 | `native-sparse-live-025-random-capacity-scan` | native | 200.0% | 16 | 25% random | capacity-scan | 19.82 | 1.63 | 13.10 | 0.00 | 0.00 | 2.217 | 0.188 | 131072 | 7.00 | 12.78 |
| 22 | `native-sparse-live-050-packed-live-bitmap` | native | 200.0% | 16 | 50% packed | live-bitmap | 22.00 | 4.65 | 14.29 | 0.00 | 0.00 | 1.141 | 0.071 | 131072 | 6.00 | 12.45 |
| 23 | `native-sparse-live-050-packed-capacity-scan` | native | 200.0% | 16 | 50% packed | capacity-scan | 20.44 | 5.72 | 13.05 | 0.00 | 0.00 | 1.342 | 0.071 | 131072 | 6.00 | 12.53 |
| 24 | `native-sparse-live-050-random-live-bitmap` | native | 200.0% | 16 | 50% random | live-bitmap | 20.52 | 1.46 | 13.68 | 0.00 | 0.00 | 1.458 | 0.104 | 131072 | 6.00 | 12.72 |
| 25 | `native-sparse-live-050-random-capacity-scan` | native | 200.0% | 16 | 50% random | capacity-scan | 22.04 | 2.51 | 14.41 | 0.00 | 0.00 | 1.588 | 0.043 | 131072 | 6.00 | 12.78 |
| 26 | `native-sparse-live-075-packed-live-bitmap` | native | 200.0% | 16 | 75% packed | live-bitmap | 21.04 | 2.33 | 14.53 | 0.00 | 0.00 | 1.119 | 0.019 | 131072 | 5.00 | 12.34 |
| 27 | `native-sparse-live-075-packed-capacity-scan` | native | 200.0% | 16 | 75% packed | capacity-scan | 19.49 | 2.26 | 12.72 | 0.00 | 0.00 | 1.268 | 0.043 | 131072 | 5.00 | 12.42 |
| 28 | `native-sparse-live-075-random-live-bitmap` | native | 200.0% | 16 | 75% random | live-bitmap | 21.39 | 7.36 | 13.93 | 0.00 | 0.00 | 1.245 | 0.108 | 131072 | 5.00 | 12.72 |
| 29 | `native-sparse-live-075-random-capacity-scan` | native | 200.0% | 16 | 75% random | capacity-scan | 24.28 | 3.95 | 14.31 | 0.00 | 0.00 | 1.308 | 0.060 | 131072 | 5.00 | 12.80 |
| 30 | `native-sparse-live-090-packed-live-bitmap` | native | 200.0% | 16 | 90% packed | live-bitmap | 20.98 | 1.87 | 13.56 | 0.00 | 0.00 | 1.159 | 0.066 | 131072 | 4.40 | 12.31 |
| 31 | `native-sparse-live-090-packed-capacity-scan` | native | 200.0% | 16 | 90% packed | capacity-scan | 22.46 | 2.04 | 14.77 | 0.00 | 0.00 | 1.226 | 0.078 | 131072 | 4.40 | 12.38 |
| 32 | `native-sparse-live-090-random-live-bitmap` | native | 200.0% | 16 | 90% random | live-bitmap | 23.60 | 3.58 | 16.26 | 0.00 | 0.00 | 1.176 | 0.033 | 131072 | 4.40 | 12.73 |
| 33 | `native-sparse-live-090-random-capacity-scan` | native | 200.0% | 16 | 90% random | capacity-scan | 20.90 | 0.96 | 13.86 | 0.00 | 0.00 | 1.273 | 0.046 | 131072 | 4.40 | 12.78 |
| 34 | `native-sparse-live-100-packed-live-bitmap` | native | 200.0% | 16 | 100% packed | live-bitmap | 19.60 | 3.00 | 12.94 | 0.00 | 0.00 | 1.122 | 0.067 | 131072 | 4.00 | 12.22 |
| 35 | `native-sparse-live-100-packed-capacity-scan` | native | 200.0% | 16 | 100% packed | capacity-scan | 22.44 | 4.55 | 14.40 | 0.00 | 0.00 | 1.208 | 0.024 | 131072 | 4.00 | 12.28 |
## native chunk boundary

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 36 | `native-boundary-minus-one` | native | 100.0% | 16 | 100% packed | live-bitmap | 21.97 | 3.24 | 18.58 | 0.00 | 0.00 | 1.145 | 0.108 | 65536 | 0.00 | 7.88 |
| 37 | `native-boundary-exact` | native | 100.0% | 16 | 100% packed | live-bitmap | 20.17 | 3.24 | 16.49 | 0.00 | 0.00 | 1.035 | 0.067 | 65536 | 0.00 | 7.88 |
| 38 | `native-boundary-plus-one` | native | 100.0% | 16 | 100% packed | live-bitmap | 20.12 | 0.94 | 16.68 | 5.21 | 5.21 | 1.094 | 0.067 | 66560 | 0.06 | 7.95 |
## Wasm forecast error

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 39 | `wasm-forecast-hint-050` | wasm | 50.0% | 16 | 100% packed | live-bitmap | 7.87 | 0.10 | 7.78 | 2.88 | 2.88 | 1.291 | 0.039 | 65536 | 0.00 | 13.16 |
| 40 | `wasm-forecast-hint-075` | wasm | 75.0% | 16 | 100% packed | live-bitmap | 8.06 | 0.52 | 8.02 | 1.96 | 1.96 | 1.285 | 0.018 | 65536 | 0.00 | 13.17 |
| 41 | `wasm-forecast-hint-100` | wasm | 100.0% | 16 | 100% packed | live-bitmap | 7.20 | 0.24 | 7.16 | 0.00 | 0.00 | 1.281 | 0.007 | 65536 | 0.00 | 13.17 |
| 42 | `wasm-forecast-hint-125` | wasm | 125.0% | 16 | 100% packed | live-bitmap | 7.54 | 0.99 | 7.49 | 0.00 | 0.00 | 1.279 | 0.026 | 81920 | 1.00 | 13.17 |
| 43 | `wasm-forecast-hint-200` | wasm | 200.0% | 16 | 100% packed | live-bitmap | 7.20 | 0.39 | 7.16 | 0.00 | 0.00 | 1.295 | 0.039 | 131072 | 4.00 | 13.17 |
| 44 | `wasm-forecast-hint-400` | wasm | 400.0% | 16 | 100% packed | live-bitmap | 7.46 | 2.24 | 7.43 | 0.00 | 0.00 | 1.281 | 0.032 | 262144 | 12.00 | 13.17 |
## Wasm growth chunk

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 45 | `wasm-chunk-hint-075-pages-01` | wasm | 75.0% | 1 | 100% packed | live-bitmap | 8.05 | 0.46 | 8.01 | 1.04 | 1.46 | 1.318 | 0.050 | 65536 | 0.00 | 13.19 |
| 46 | `wasm-chunk-hint-075-pages-04` | wasm | 75.0% | 4 | 100% packed | live-bitmap | 8.12 | 0.84 | 8.09 | 2.88 | 2.88 | 1.277 | 0.051 | 65536 | 0.00 | 13.19 |
| 47 | `wasm-chunk-hint-075-pages-16` | wasm | 75.0% | 16 | 100% packed | live-bitmap | 7.73 | 0.64 | 7.69 | 3.21 | 3.21 | 1.284 | 0.072 | 65536 | 0.00 | 13.19 |
| 48 | `wasm-chunk-hint-075-pages-64` | wasm | 75.0% | 64 | 100% packed | live-bitmap | 8.40 | 2.25 | 8.35 | 4.29 | 4.29 | 1.281 | 0.009 | 65536 | 0.00 | 13.16 |

## Interpretation limits

- Reserve time and spawn time are separate. Native reserve allocates stable chunks; Wasm reserve performs one host `memory.grow` to the estimated size after module/store construction.
- Spare state is not explicitly touched. It can remain lazily physically backed, so logical reserved/live byte counts carry more meaning than small RSS differences.
- Global allocation counting is disabled so its atomics cannot perturb primary timing.
- `live-bitmap` traverses a two-level live-page hierarchy and live slot words. `capacity-scan` deliberately models the failure mode that visits every reserved page.
- Wasm hot cells execute a real guest sweep over packed live state. Sparse Wasm state is excluded because it would require choosing a production live-set ABI that this capacity test is not intended to decide.
- Fresh processes prevent allocator and Wasmtime state from leaking between cells. Rotated order reduces thermal and frequency bias; medians and IQRs remain descriptive rather than inferential statistics.
- Checksums and exact update counts are verified within every cell and across cells that declare equivalent logical work.
