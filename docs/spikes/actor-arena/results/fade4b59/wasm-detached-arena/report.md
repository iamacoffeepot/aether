# Actor arena paired comparison

Base: `wasm-detached`  
Candidate: `wasm-arena`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 20.000 ns/mail |
| Candidate median | 17.468 ns/mail |
| Median paired delta | -2.454 ns/mail |
| Paired delta IQR | 0.298 ns/mail |
| Relative median change | -12.66% |
| Median speedup | 1.139× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 2.000 ns/mail |

Configuration: `dispatch` workload, 256 actors, 500000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 19.724 | 17.327 | -2.397 | 1.138× |
| 1 | CandidateBase | 19.784 | 17.205 | -2.579 | 1.150× |
| 2 | BaseCandidate | 20.441 | 17.177 | -3.264 | 1.190× |
| 3 | CandidateBase | 20.088 | 17.413 | -2.674 | 1.154× |
| 4 | BaseCandidate | 20.155 | 17.701 | -2.454 | 1.139× |
| 5 | CandidateBase | 20.000 | 17.468 | -2.532 | 1.145× |
| 6 | BaseCandidate | 19.410 | 17.591 | -1.819 | 1.103× |
| 7 | CandidateBase | 20.056 | 17.774 | -2.282 | 1.128× |
| 8 | BaseCandidate | 19.148 | 17.751 | -1.398 | 1.079× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 500000 | 31250 |
| Host entries | 500000 | 500000 |
| Host-to-guest bytes | 4000000 | 4000000 |
| Guest linear memory bytes | 16777216 | 131072 |
| Peak RSS bytes | 17154048 | 10862592 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
