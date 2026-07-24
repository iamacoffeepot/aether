# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-batch`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 17.451 ns/mail |
| Candidate median | 3.974 ns/mail |
| Median paired delta | -13.585 ns/mail |
| Paired delta IQR | 0.102 ns/mail |
| Relative median change | -77.23% |
| Median speedup | 4.405× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 1.745 ns/mail |

Configuration: `dispatch` workload, 1024 actors, 250000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 17.594 | 3.994 | -13.600 | 4.405× |
| 1 | CandidateBase | 17.451 | 3.867 | -13.585 | 4.513× |
| 2 | BaseCandidate | 17.370 | 3.974 | -13.396 | 4.371× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 15625 | 245 |
| Host entries | 250000 | 245 |
| Host-to-guest bytes | 2000000 | 4000000 |
| Guest linear memory bytes | 327680 | 327680 |
| Peak RSS bytes | 10813440 | 11206656 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
