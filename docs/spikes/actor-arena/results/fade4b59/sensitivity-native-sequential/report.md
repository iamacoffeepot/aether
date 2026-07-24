# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 1.918 ns/mail |
| Candidate median | 2.109 ns/mail |
| Median paired delta | +0.190 ns/mail |
| Paired delta IQR | 0.036 ns/mail |
| Relative median change | +9.94% |
| Median speedup | 0.909× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 5000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 1.894 | 2.109 | +0.215 | 0.898× |
| 1 | CandidateBase | 1.922 | 2.097 | +0.175 | 0.916× |
| 2 | BaseCandidate | 1.935 | 2.108 | +0.173 | 0.918× |
| 3 | CandidateBase | 1.909 | 2.141 | +0.232 | 0.892× |
| 4 | BaseCandidate | 1.918 | 2.127 | +0.209 | 0.902× |
| 5 | CandidateBase | 1.952 | 2.098 | +0.146 | 0.930× |
| 6 | BaseCandidate | 1.906 | 2.110 | +0.203 | 0.904× |
| 7 | CandidateBase | 1.901 | 2.090 | +0.190 | 0.909× |
| 8 | BaseCandidate | 1.947 | 2.114 | +0.167 | 0.921× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 312500 | 312500 |
| State lock acquisitions | 312500 | 312500 |
| Scheduled items | 312500 | 312500 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 7143424 | 6963200 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
