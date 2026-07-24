# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 2.594 ns/mail |
| Candidate median | 2.690 ns/mail |
| Median paired delta | +0.119 ns/mail |
| Paired delta IQR | 0.161 ns/mail |
| Relative median change | +3.72% |
| Median speedup | 0.956× |
| Directional consistency | 66.7% |
| ADR-0085 noise floor | 0.300 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 5000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 3.898 | 3.380 | -0.518 | 1.153× |
| 1 | CandidateBase | 2.594 | 2.951 | +0.357 | 0.879× |
| 2 | BaseCandidate | 2.441 | 2.658 | +0.217 | 0.918× |
| 3 | CandidateBase | 2.810 | 2.819 | +0.009 | 0.997× |
| 4 | BaseCandidate | 2.989 | 2.689 | -0.300 | 1.112× |
| 5 | CandidateBase | 2.558 | 2.677 | +0.119 | 0.956× |
| 6 | BaseCandidate | 2.593 | 2.722 | +0.129 | 0.953× |
| 7 | CandidateBase | 2.702 | 2.690 | -0.012 | 1.004× |
| 8 | BaseCandidate | 2.513 | 2.662 | +0.149 | 0.944× |

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
