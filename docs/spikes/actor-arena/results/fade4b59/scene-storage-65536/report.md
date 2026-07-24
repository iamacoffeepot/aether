# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 6.756 ns/entity update |
| Candidate median | 8.045 ns/entity update |
| Median paired delta | +1.357 ns/entity update |
| Paired delta IQR | 0.186 ns/entity update |
| Relative median change | +19.08% |
| Median speedup | 0.831× |
| Directional consistency | 88.9% |
| ADR-0085 noise floor | 0.676 ns/mail |

Configuration: `scene-sweep` workload, 65536 actors, 5000000 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 6.773 | 8.297 | +1.524 | 0.816× |
| 1 | CandidateBase | 6.571 | 8.134 | +1.563 | 0.808× |
| 2 | BaseCandidate | 6.480 | 7.979 | +1.499 | 0.812× |
| 3 | CandidateBase | 6.756 | 8.073 | +1.316 | 0.837× |
| 4 | BaseCandidate | 6.783 | 8.003 | +1.220 | 0.848× |
| 5 | CandidateBase | 8.770 | 7.931 | -0.839 | 1.106× |
| 6 | BaseCandidate | 6.978 | 8.481 | +1.503 | 0.823× |
| 7 | CandidateBase | 6.688 | 8.045 | +1.357 | 0.831× |
| 8 | BaseCandidate | 6.682 | 8.006 | +1.324 | 0.835× |

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
| Peak RSS bytes | 62062592 | 58966016 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
