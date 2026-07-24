# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-batch`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 17.461 ns/mail |
| Candidate median | 3.995 ns/mail |
| Median paired delta | -13.477 ns/mail |
| Paired delta IQR | 0.176 ns/mail |
| Relative median change | -77.12% |
| Median speedup | 4.385× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 1.746 ns/mail |

Configuration: `dispatch` workload, 1024 actors, 1000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 17.527 | 3.927 | -13.600 | 4.463× |
| 1 | CandidateBase | 17.596 | 4.013 | -13.583 | 4.385× |
| 2 | BaseCandidate | 17.430 | 3.953 | -13.477 | 4.409× |
| 3 | CandidateBase | 17.834 | 3.960 | -13.874 | 4.504× |
| 4 | BaseCandidate | 17.461 | 3.995 | -13.465 | 4.370× |
| 5 | CandidateBase | 17.419 | 4.038 | -13.381 | 4.314× |
| 6 | BaseCandidate | 17.354 | 3.988 | -13.366 | 4.352× |
| 7 | CandidateBase | 17.427 | 4.002 | -13.424 | 4.354× |
| 8 | BaseCandidate | 17.804 | 4.000 | -13.804 | 4.451× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 62500 | 977 |
| Host entries | 1000000 | 977 |
| Host-to-guest bytes | 8000000 | 16000000 |
| Guest linear memory bytes | 327680 | 327680 |
| Peak RSS bytes | 11599872 | 11321344 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
