# Actor arena paired comparison

Base: `arena-endpoint`  
Candidate: `arena-page`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 7.572 ns/entity update |
| Candidate median | 3.020 ns/entity update |
| Median paired delta | -4.542 ns/entity update |
| Paired delta IQR | 0.143 ns/entity update |
| Relative median change | -60.12% |
| Median speedup | 2.521× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.757 ns/mail |

Configuration: `scene-sweep` workload, 65536 actors, 5000000 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 7.624 | 3.314 | -4.311 | 2.301× |
| 1 | CandidateBase | 7.700 | 3.022 | -4.678 | 2.548× |
| 2 | BaseCandidate | 7.572 | 3.029 | -4.542 | 2.500× |
| 3 | CandidateBase | 7.624 | 2.977 | -4.647 | 2.561× |
| 4 | BaseCandidate | 7.580 | 2.899 | -4.681 | 2.615× |
| 5 | CandidateBase | 7.518 | 2.982 | -4.536 | 2.521× |
| 6 | BaseCandidate | 7.469 | 2.886 | -4.583 | 2.588× |
| 7 | CandidateBase | 7.553 | 3.294 | -4.259 | 2.293× |
| 8 | BaseCandidate | 7.524 | 3.020 | -4.505 | 2.492× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 5000000 | 78125 |
| Scheduled items | 5000000 | 78125 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 51380224 | 52477952 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
