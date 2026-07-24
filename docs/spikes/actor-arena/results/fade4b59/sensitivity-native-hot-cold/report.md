# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 2.069 ns/mail |
| Candidate median | 2.265 ns/mail |
| Median paired delta | +0.191 ns/mail |
| Paired delta IQR | 0.037 ns/mail |
| Relative median change | +9.46% |
| Median speedup | 0.916× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 5000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `hot-cold` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 2.112 | 2.297 | +0.184 | 0.920× |
| 1 | CandidateBase | 2.060 | 2.281 | +0.221 | 0.903× |
| 2 | BaseCandidate | 2.069 | 2.265 | +0.196 | 0.914× |
| 3 | CandidateBase | 2.071 | 2.262 | +0.191 | 0.916× |
| 4 | BaseCandidate | 2.078 | 2.265 | +0.187 | 0.917× |
| 5 | CandidateBase | 2.082 | 2.265 | +0.183 | 0.919× |
| 6 | BaseCandidate | 2.063 | 2.305 | +0.242 | 0.895× |
| 7 | CandidateBase | 2.069 | 2.233 | +0.164 | 0.927× |
| 8 | BaseCandidate | 2.067 | 2.319 | +0.253 | 0.891× |

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
| Peak RSS bytes | 7143424 | 6946816 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
