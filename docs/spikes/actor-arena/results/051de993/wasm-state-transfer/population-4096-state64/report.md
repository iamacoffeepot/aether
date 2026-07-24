# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 1.568 ns/entity update |
| Candidate median | 3.116 ns/entity update |
| Median paired delta | +1.551 ns/entity update |
| Paired delta IQR | 0.101 ns/entity update |
| Relative median change | +98.70% |
| Median speedup | 0.499× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/entity update |

Configuration: `scene-sweep` workload, 4096 actors, 5242880 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 1.568 | 3.162 | +1.594 | 0.496× |
| 1 | CandidateBase | 1.561 | 3.077 | +1.515 | 0.507× |
| 2 | BaseCandidate | 1.590 | 3.194 | +1.604 | 0.498× |
| 3 | CandidateBase | 1.643 | 3.116 | +1.473 | 0.527× |
| 4 | BaseCandidate | 1.603 | 3.083 | +1.480 | 0.520× |
| 5 | CandidateBase | 1.547 | 3.098 | +1.551 | 0.499× |
| 6 | BaseCandidate | 1.548 | 3.192 | +1.643 | 0.485× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 1280 | 1280 |
| Host entries | 1280 | 1280 |
| Host-to-guest bytes | 0 | 335544320 |
| Guest-to-host bytes | 0 | 335544320 |
| State round trips | 0 | 1280 |
| Guest linear memory bytes | 327680 | 327680 |
| Peak RSS bytes | 53133312 | 53854208 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
