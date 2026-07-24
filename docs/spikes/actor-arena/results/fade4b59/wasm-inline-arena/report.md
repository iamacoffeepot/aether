# Actor arena paired comparison

Base: `wasm-inline`  
Candidate: `wasm-arena`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 17.788 ns/mail |
| Candidate median | 17.678 ns/mail |
| Median paired delta | -0.070 ns/mail |
| Paired delta IQR | 0.344 ns/mail |
| Relative median change | -0.62% |
| Median speedup | 1.004× |
| Directional consistency | 66.7% |
| ADR-0085 noise floor | 1.779 ns/mail |

Configuration: `dispatch` workload, 1024 actors, 1000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 17.464 | 17.622 | +0.158 | 0.991× |
| 1 | CandidateBase | 17.931 | 17.678 | -0.253 | 1.014× |
| 2 | BaseCandidate | 18.196 | 17.783 | -0.413 | 1.023× |
| 3 | CandidateBase | 17.983 | 17.926 | -0.056 | 1.003× |
| 4 | BaseCandidate | 17.796 | 17.610 | -0.186 | 1.011× |
| 5 | CandidateBase | 17.511 | 17.789 | +0.278 | 0.984× |
| 6 | BaseCandidate | 17.609 | 17.539 | -0.070 | 1.004× |
| 7 | CandidateBase | 17.788 | 17.614 | -0.173 | 1.010× |
| 8 | BaseCandidate | 17.778 | 17.950 | +0.172 | 0.990× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 62500 | 62500 |
| Host entries | 1000000 | 1000000 |
| Host-to-guest bytes | 8000000 | 8000000 |
| Guest linear memory bytes | 393216 | 327680 |
| Peak RSS bytes | 11452416 | 11354112 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
