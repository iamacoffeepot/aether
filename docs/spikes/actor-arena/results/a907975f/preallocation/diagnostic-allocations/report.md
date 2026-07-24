# Actor arena preallocation matrix

Each cell has 3 fresh-process samples. Cell order rotates by an evenly distributed stride and alternates forward/reverse between rounds. Cold rates include capacity reservation plus actor state initialization. Hot rates follow a warm/reset phase and contain bullet updates only.

Forced reserved-page touching: **disabled**. Allocation instrumentation: **enabled**.

## allocation diagnostic

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | `diagnostic-native-hint-050` | native | 50.0% | 16 | 100% packed | live-bitmap | 16.33 | 0.86 | 14.40 | 5.88 | 5.88 | 1.114 | 0.025 | 65536 | 0.00 | 7.92 |
| 1 | `diagnostic-native-hint-100` | native | 100.0% | 16 | 100% packed | live-bitmap | 16.30 | 2.19 | 13.08 | 0.00 | 0.00 | 1.143 | 0.056 | 65536 | 0.00 | 7.91 |
| 2 | `diagnostic-native-hint-200` | native | 200.0% | 16 | 100% packed | live-bitmap | 22.48 | 1.88 | 15.29 | 0.00 | 0.00 | 1.169 | 0.167 | 131072 | 4.00 | 12.27 |
| 3 | `diagnostic-native-pages-01` | native | 75.0% | 1 | 100% packed | live-bitmap | 17.19 | 2.35 | 13.25 | 1.92 | 3.58 | 1.171 | 0.011 | 65536 | 0.00 | 8.08 |
| 4 | `diagnostic-native-pages-16` | native | 75.0% | 16 | 100% packed | live-bitmap | 16.93 | 1.71 | 14.35 | 9.38 | 9.38 | 1.119 | 0.086 | 65536 | 0.00 | 7.91 |
| 5 | `diagnostic-native-pages-64` | native | 75.0% | 64 | 100% packed | live-bitmap | 16.16 | 1.22 | 13.62 | 19.38 | 19.38 | 1.133 | 0.024 | 65536 | 0.00 | 7.91 |
| 6 | `diagnostic-wasm-hint-050` | wasm | 50.0% | 16 | 100% packed | live-bitmap | 8.24 | 0.75 | 8.19 | 7.00 | 7.00 | 1.254 | 0.022 | 65536 | 0.00 | 13.19 |
| 7 | `diagnostic-wasm-hint-100` | wasm | 100.0% | 16 | 100% packed | live-bitmap | 9.13 | 1.10 | 9.10 | 0.00 | 0.00 | 1.253 | 0.045 | 65536 | 0.00 | 13.17 |
| 8 | `diagnostic-wasm-hint-200` | wasm | 200.0% | 16 | 100% packed | live-bitmap | 8.03 | 0.18 | 7.98 | 0.00 | 0.00 | 1.287 | 0.088 | 131072 | 4.00 | 13.16 |

## Interpretation limits

- Reserve time and spawn time are separate. Native reserve allocates stable chunks; Wasm reserve performs one host `memory.grow` to the estimated size after module/store construction.
- Spare state is not explicitly touched. It can remain lazily physically backed, so logical reserved/live byte counts carry more meaning than small RSS differences.
- Global allocation counting is enabled only around reserve and spawn. Its atomics intentionally make this a diagnostic rather than a primary timing pass.
- `live-bitmap` traverses a two-level live-page hierarchy and live slot words. `capacity-scan` deliberately models the failure mode that visits every reserved page.
- Wasm hot cells execute a real guest sweep over packed live state. Sparse Wasm state is excluded because it would require choosing a production live-set ABI that this capacity test is not intended to decide.
- Fresh processes prevent allocator and Wasmtime state from leaking between cells. Rotated order reduces thermal and frequency bias; medians and IQRs remain descriptive rather than inferential statistics.
- Checksums and exact update counts are verified within every cell and across cells that declare equivalent logical work.
