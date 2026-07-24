# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 1.679 ns/entity update |
| Candidate median | 3.186 ns/entity update |
| Median paired delta | +1.510 ns/entity update |
| Paired delta IQR | 0.032 ns/entity update |
| Relative median change | +89.79% |
| Median speedup | 0.526× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/entity update |

Configuration: `scene-sweep` workload, 131072 actors, 5242880 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 1.679 | 3.192 | +1.513 | 0.526× |
| 1 | CandidateBase | 1.676 | 3.186 | +1.510 | 0.526× |
| 2 | BaseCandidate | 1.691 | 3.253 | +1.561 | 0.520× |
| 3 | CandidateBase | 1.692 | 3.182 | +1.490 | 0.532× |
| 4 | BaseCandidate | 1.728 | 3.188 | +1.459 | 0.542× |
| 5 | CandidateBase | 1.659 | 3.133 | +1.474 | 0.530× |
| 6 | BaseCandidate | 1.665 | 3.181 | +1.515 | 0.524× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 40 | 40 |
| Host entries | 40 | 40 |
| Host-to-guest bytes | 0 | 335544320 |
| Guest-to-host bytes | 0 | 335544320 |
| State round trips | 0 | 40 |
| Guest linear memory bytes | 8454144 | 8454144 |
| Peak RSS bytes | 62783488 | 79396864 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
