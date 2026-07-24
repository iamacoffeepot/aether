# Actor arena paired comparison

Base: `arena-endpoint`  
Candidate: `arena-page`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 2.353 ns/mail |
| Candidate median | 2.269 ns/mail |
| Median paired delta | -0.060 ns/mail |
| Paired delta IQR | 0.187 ns/mail |
| Relative median change | -3.56% |
| Median speedup | 1.027× |
| Directional consistency | 77.8% |
| ADR-0085 noise floor | 0.300 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 5000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 2.465 | 2.271 | -0.195 | 1.086× |
| 1 | CandidateBase | 2.353 | 2.569 | +0.216 | 0.916× |
| 2 | BaseCandidate | 2.304 | 2.254 | -0.051 | 1.022× |
| 3 | CandidateBase | 2.272 | 2.212 | -0.060 | 1.027× |
| 4 | BaseCandidate | 2.478 | 2.346 | -0.132 | 1.056× |
| 5 | CandidateBase | 2.476 | 2.268 | -0.209 | 1.092× |
| 6 | BaseCandidate | 2.890 | 2.422 | -0.468 | 1.193× |
| 7 | CandidateBase | 2.261 | 2.269 | +0.008 | 0.996× |
| 8 | BaseCandidate | 2.247 | 2.239 | -0.007 | 1.003× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 312500 | 312500 |
| State lock acquisitions | 312500 | 198523 |
| Scheduled items | 312500 | 198523 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 6422528 | 6520832 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
