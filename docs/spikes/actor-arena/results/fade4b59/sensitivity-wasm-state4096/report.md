# Actor arena paired comparison

Base: `wasm-inline`  
Candidate: `wasm-arena`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 17.742 ns/mail |
| Candidate median | 17.825 ns/mail |
| Median paired delta | -0.043 ns/mail |
| Paired delta IQR | 0.831 ns/mail |
| Relative median change | +0.47% |
| Median speedup | 1.002× |
| Directional consistency | 55.6% |
| ADR-0085 noise floor | 1.774 ns/mail |

Configuration: `dispatch` workload, 1024 actors, 1000000 work units, 4096 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 17.678 | 18.472 | +0.793 | 0.957× |
| 1 | CandidateBase | 18.020 | 17.825 | -0.195 | 1.011× |
| 2 | BaseCandidate | 17.774 | 17.731 | -0.043 | 1.002× |
| 3 | CandidateBase | 17.819 | 17.619 | -0.201 | 1.011× |
| 4 | BaseCandidate | 17.631 | 18.103 | +0.472 | 0.974× |
| 5 | CandidateBase | 17.561 | 18.263 | +0.702 | 0.962× |
| 6 | BaseCandidate | 17.742 | 18.831 | +1.089 | 0.942× |
| 7 | CandidateBase | 17.691 | 17.646 | -0.045 | 1.003× |
| 8 | BaseCandidate | 17.826 | 17.697 | -0.129 | 1.007× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 62500 | 62500 |
| Host entries | 1000000 | 1000000 |
| Host-to-guest bytes | 8000000 | 8000000 |
| Guest linear memory bytes | 4325376 | 4259840 |
| Peak RSS bytes | 15237120 | 15286272 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
