# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 1.551 ns/entity update |
| Candidate median | 3.157 ns/entity update |
| Median paired delta | +1.641 ns/entity update |
| Paired delta IQR | 0.151 ns/entity update |
| Relative median change | +103.58% |
| Median speedup | 0.479× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/entity update |

Configuration: `scene-sweep` workload, 16384 actors, 5242880 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 1.659 | 2.999 | +1.341 | 0.553× |
| 1 | CandidateBase | 1.567 | 3.094 | +1.527 | 0.506× |
| 2 | BaseCandidate | 1.524 | 3.548 | +2.024 | 0.429× |
| 3 | CandidateBase | 1.507 | 3.148 | +1.641 | 0.479× |
| 4 | BaseCandidate | 1.551 | 3.157 | +1.606 | 0.491× |
| 5 | CandidateBase | 1.573 | 3.307 | +1.734 | 0.476× |
| 6 | BaseCandidate | 1.502 | 3.203 | +1.701 | 0.469× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 320 | 320 |
| Host entries | 320 | 320 |
| Host-to-guest bytes | 0 | 335544320 |
| Guest-to-host bytes | 0 | 335544320 |
| State round trips | 0 | 320 |
| Guest linear memory bytes | 1114112 | 1114112 |
| Peak RSS bytes | 54607872 | 57720832 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
