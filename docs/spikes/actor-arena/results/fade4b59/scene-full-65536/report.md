# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-page`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 6.686 ns/entity update |
| Candidate median | 3.073 ns/entity update |
| Median paired delta | -3.676 ns/entity update |
| Paired delta IQR | 0.290 ns/entity update |
| Relative median change | -54.04% |
| Median speedup | 2.182× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.669 ns/mail |

Configuration: `scene-sweep` workload, 65536 actors, 5000000 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 6.552 | 3.125 | -3.427 | 2.097× |
| 1 | CandidateBase | 6.573 | 2.897 | -3.676 | 2.269× |
| 2 | BaseCandidate | 6.584 | 3.238 | -3.346 | 2.033× |
| 3 | CandidateBase | 6.686 | 4.515 | -2.171 | 1.481× |
| 4 | BaseCandidate | 6.873 | 3.155 | -3.718 | 2.178× |
| 5 | CandidateBase | 6.704 | 3.073 | -3.631 | 2.182× |
| 6 | BaseCandidate | 6.934 | 3.050 | -3.884 | 2.273× |
| 7 | CandidateBase | 6.681 | 2.994 | -3.687 | 2.231× |
| 8 | BaseCandidate | 6.727 | 3.002 | -3.725 | 2.241× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 5000000 | 78125 |
| Scheduled items | 5000000 | 78125 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 61997056 | 52428800 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
