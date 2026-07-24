# Actor arena preallocation matrix

Each cell has 3 fresh-process samples. Cell order rotates by an evenly distributed stride and alternates forward/reverse between rounds. Cold rates include capacity reservation plus actor state initialization. Hot rates follow a warm/reset phase and contain bullet updates only.

Forced reserved-page touching: **enabled**. Allocation instrumentation: **disabled**.

## forced page-touch diagnostic

| # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |
|---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | `diagnostic-native-hint-050` | native | 50.0% | 16 | 100% packed | live-bitmap | 16.31 | 2.18 | 14.62 | 8.58 | 8.58 | 1.105 | 0.020 | 65536 | 0.00 | 7.91 |
| 1 | `diagnostic-native-hint-100` | native | 100.0% | 16 | 100% packed | live-bitmap | 15.41 | 0.74 | 12.15 | 0.00 | 0.00 | 1.123 | 0.022 | 65536 | 0.00 | 7.89 |
| 2 | `diagnostic-native-hint-200` | native | 200.0% | 16 | 100% packed | live-bitmap | 21.17 | 3.91 | 14.30 | 0.00 | 0.00 | 1.133 | 0.014 | 131072 | 4.00 | 12.28 |
| 3 | `diagnostic-native-pages-01` | native | 75.0% | 1 | 100% packed | live-bitmap | 17.04 | 2.31 | 13.05 | 1.46 | 2.58 | 1.159 | 0.010 | 65536 | 0.00 | 8.08 |
| 4 | `diagnostic-native-pages-16` | native | 75.0% | 16 | 100% packed | live-bitmap | 18.94 | 0.93 | 16.28 | 5.42 | 5.42 | 1.116 | 0.010 | 65536 | 0.00 | 7.89 |
| 5 | `diagnostic-native-pages-64` | native | 75.0% | 64 | 100% packed | live-bitmap | 15.11 | 1.70 | 12.54 | 15.08 | 15.08 | 1.105 | 0.042 | 65536 | 0.00 | 7.92 |
| 6 | `diagnostic-wasm-hint-050` | wasm | 50.0% | 16 | 100% packed | live-bitmap | 7.96 | 0.09 | 6.23 | 4.46 | 4.46 | 1.239 | 0.017 | 65536 | 0.00 | 13.20 |
| 7 | `diagnostic-wasm-hint-100` | wasm | 100.0% | 16 | 100% packed | live-bitmap | 7.27 | 0.20 | 4.25 | 0.00 | 0.00 | 1.289 | 0.052 | 65536 | 0.00 | 13.20 |
| 8 | `diagnostic-wasm-hint-200` | wasm | 200.0% | 16 | 100% packed | live-bitmap | 12.09 | 0.95 | 4.30 | 0.00 | 0.00 | 1.417 | 0.113 | 131072 | 4.00 | 17.17 |

## Interpretation limits

- Reserve time and spawn time are separate. Native reserve allocates stable chunks; Wasm reserve performs one host `memory.grow` to the estimated size after module/store construction.
- Reserved state is forcibly touched once per 4 KiB host page before actor initialization. This diagnoses physical commitment and intentionally perturbs cold timing.
- Global allocation counting is disabled so its atomics cannot perturb primary timing.
- `live-bitmap` traverses a two-level live-page hierarchy and live slot words. `capacity-scan` deliberately models the failure mode that visits every reserved page.
- Wasm hot cells execute a real guest sweep over packed live state. Sparse Wasm state is excluded because it would require choosing a production live-set ABI that this capacity test is not intended to decide.
- Fresh processes prevent allocator and Wasmtime state from leaking between cells. Rotated order reduces thermal and frequency bias; medians and IQRs remain descriptive rather than inferential statistics.
- Checksums and exact update counts are verified within every cell and across cells that declare equivalent logical work.
