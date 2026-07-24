# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 37.335 ns/lifecycle op |
| Candidate median | 19.448 ns/lifecycle op |
| Median paired delta | -17.704 ns/lifecycle op |
| Paired delta IQR | 1.974 ns/lifecycle op |
| Relative median change | -47.91% |
| Median speedup | 1.915× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 3.734 ns/mail |

Configuration: `lifecycle-churn` workload, 4096 actors, 1000000 work units, 256 bytes/state, 1 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 40.619 | 19.675 | -20.944 | 2.064× |
| 1 | CandidateBase | 37.053 | 19.349 | -17.704 | 1.915× |
| 2 | BaseCandidate | 40.083 | 19.340 | -20.743 | 2.073× |
| 3 | CandidateBase | 37.114 | 19.448 | -17.666 | 1.908× |
| 4 | BaseCandidate | 36.708 | 19.324 | -17.383 | 1.900× |
| 5 | CandidateBase | 36.934 | 19.283 | -17.651 | 1.915× |
| 6 | BaseCandidate | 37.335 | 20.210 | -17.125 | 1.847× |
| 7 | CandidateBase | 38.446 | 19.948 | -18.498 | 1.927× |
| 8 | BaseCandidate | 40.864 | 21.239 | -19.625 | 1.924× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 1000000 | 1000000 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 12713984 | 11665408 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
