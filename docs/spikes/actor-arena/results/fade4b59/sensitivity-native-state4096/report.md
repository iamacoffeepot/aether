# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 3.177 ns/mail |
| Candidate median | 2.982 ns/mail |
| Median paired delta | -0.158 ns/mail |
| Paired delta IQR | 0.470 ns/mail |
| Relative median change | -6.13% |
| Median speedup | 1.052× |
| Directional consistency | 66.7% |
| ADR-0085 noise floor | 0.705 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 5000000 work units, 4096 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 3.369 | 2.982 | -0.387 | 1.130× |
| 1 | CandidateBase | 2.796 | 2.897 | +0.101 | 0.965× |
| 2 | BaseCandidate | 3.328 | 2.960 | -0.369 | 1.125× |
| 3 | CandidateBase | 3.343 | 2.945 | -0.398 | 1.135× |
| 4 | BaseCandidate | 3.221 | 3.068 | -0.153 | 1.050× |
| 5 | CandidateBase | 3.177 | 3.019 | -0.158 | 1.052× |
| 6 | BaseCandidate | 2.762 | 3.156 | +0.394 | 0.875× |
| 7 | CandidateBase | 2.961 | 3.211 | +0.250 | 0.922× |
| 8 | BaseCandidate | 3.132 | 2.938 | -0.194 | 1.066× |

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
| Peak RSS bytes | 22937600 | 22757376 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
