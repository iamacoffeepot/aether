# Actor arena paired comparison

Base: `wasm-detached`  
Candidate: `wasm-arena`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 18.687 ns/mail |
| Candidate median | 16.726 ns/mail |
| Median paired delta | -1.293 ns/mail |
| Paired delta IQR | 1.195 ns/mail |
| Relative median change | -10.49% |
| Median speedup | 1.077× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 1.869 ns/mail |

Configuration: `dispatch` workload, 64 actors, 100000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 20.760 | 16.726 | -4.034 | 1.241× |
| 1 | CandidateBase | 19.017 | 16.704 | -2.313 | 1.138× |
| 2 | BaseCandidate | 18.687 | 17.568 | -1.118 | 1.064× |
| 3 | CandidateBase | 18.002 | 16.709 | -1.293 | 1.077× |
| 4 | BaseCandidate | 18.098 | 17.340 | -0.759 | 1.044× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 100000 | 6250 |
| Host entries | 100000 | 100000 |
| Host-to-guest bytes | 800000 | 800000 |
| Guest linear memory bytes | 4194304 | 65536 |
| Peak RSS bytes | 12042240 | 10993664 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
