# Actor arena paired comparison

Base: `arena-state`  
Candidate: `arena-endpoint`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 8.041 ns/entity update |
| Candidate median | 7.555 ns/entity update |
| Median paired delta | -0.482 ns/entity update |
| Paired delta IQR | 0.055 ns/entity update |
| Relative median change | -6.05% |
| Median speedup | 1.063× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.804 ns/mail |

Configuration: `scene-sweep` workload, 65536 actors, 5000000 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 8.020 | 7.486 | -0.534 | 1.071× |
| 1 | CandidateBase | 8.041 | 7.555 | -0.487 | 1.064× |
| 2 | BaseCandidate | 8.052 | 7.555 | -0.497 | 1.066× |
| 3 | CandidateBase | 8.014 | 7.560 | -0.455 | 1.060× |
| 4 | BaseCandidate | 8.092 | 7.650 | -0.442 | 1.058× |
| 5 | CandidateBase | 7.932 | 7.534 | -0.399 | 1.053× |
| 6 | BaseCandidate | 7.920 | 7.541 | -0.380 | 1.050× |
| 7 | CandidateBase | 8.257 | 7.634 | -0.623 | 1.082× |
| 8 | BaseCandidate | 8.133 | 7.651 | -0.482 | 1.063× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 5000000 | 5000000 |
| Scheduled items | 5000000 | 5000000 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 58966016 | 51380224 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
