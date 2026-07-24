# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 1.676 ns/entity update |
| Candidate median | 3.200 ns/entity update |
| Median paired delta | +1.523 ns/entity update |
| Paired delta IQR | 0.078 ns/entity update |
| Relative median change | +90.95% |
| Median speedup | 0.523× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/entity update |

Configuration: `scene-sweep` workload, 65536 actors, 5242880 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 2.459 | 3.545 | +1.086 | 0.694× |
| 1 | CandidateBase | 1.666 | 3.184 | +1.518 | 0.523× |
| 2 | BaseCandidate | 1.640 | 3.200 | +1.560 | 0.513× |
| 3 | CandidateBase | 1.660 | 3.213 | +1.554 | 0.516× |
| 4 | BaseCandidate | 1.688 | 3.273 | +1.585 | 0.516× |
| 5 | CandidateBase | 1.653 | 3.176 | +1.523 | 0.521× |
| 6 | BaseCandidate | 1.691 | 3.166 | +1.475 | 0.534× |
| 7 | CandidateBase | 1.676 | 3.202 | +1.526 | 0.523× |
| 8 | BaseCandidate | 1.692 | 3.145 | +1.453 | 0.538× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 80 | 80 |
| Host entries | 80 | 80 |
| Host-to-guest bytes | 0 | 335544320 |
| Guest-to-host bytes | 0 | 335544320 |
| State round trips | 0 | 80 |
| Guest linear memory bytes | 4259840 | 4259840 |
| Peak RSS bytes | 58114048 | 66551808 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
