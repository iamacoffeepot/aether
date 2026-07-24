# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 1.691 ns/entity update |
| Candidate median | 3.213 ns/entity update |
| Median paired delta | +1.514 ns/entity update |
| Paired delta IQR | 0.029 ns/entity update |
| Relative median change | +90.08% |
| Median speedup | 0.529× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/entity update |

Configuration: `scene-sweep` workload, 65536 actors, 52428800 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 1.683 | 3.218 | +1.535 | 0.523× |
| 1 | CandidateBase | 1.700 | 3.200 | +1.500 | 0.531× |
| 2 | BaseCandidate | 1.703 | 3.202 | +1.499 | 0.532× |
| 3 | CandidateBase | 1.689 | 3.175 | +1.487 | 0.532× |
| 4 | BaseCandidate | 1.691 | 3.217 | +1.526 | 0.526× |
| 5 | CandidateBase | 1.700 | 3.213 | +1.514 | 0.529× |
| 6 | BaseCandidate | 1.683 | 3.244 | +1.561 | 0.519× |
| 7 | CandidateBase | 1.691 | 3.172 | +1.482 | 0.533× |
| 8 | BaseCandidate | 1.695 | 3.223 | +1.527 | 0.526× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 800 | 800 |
| Host entries | 800 | 800 |
| Host-to-guest bytes | 0 | 3355443200 |
| Guest-to-host bytes | 0 | 3355443200 |
| State round trips | 0 | 800 |
| Guest linear memory bytes | 4259840 | 4259840 |
| Peak RSS bytes | 435519488 | 443645952 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
