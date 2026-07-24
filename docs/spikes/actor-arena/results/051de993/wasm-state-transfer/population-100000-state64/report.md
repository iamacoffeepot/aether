# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 1.665 ns/entity update |
| Candidate median | 3.194 ns/entity update |
| Median paired delta | +1.536 ns/entity update |
| Paired delta IQR | 0.016 ns/entity update |
| Relative median change | +91.84% |
| Median speedup | 0.519× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/entity update |

Configuration: `scene-sweep` workload, 100000 actors, 5000000 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 1.658 | 3.194 | +1.536 | 0.519× |
| 1 | CandidateBase | 1.659 | 3.205 | +1.546 | 0.518× |
| 2 | BaseCandidate | 1.692 | 3.222 | +1.529 | 0.525× |
| 3 | CandidateBase | 1.665 | 3.190 | +1.526 | 0.522× |
| 4 | BaseCandidate | 1.693 | 3.156 | +1.463 | 0.537× |
| 5 | CandidateBase | 1.666 | 3.221 | +1.555 | 0.517× |
| 6 | BaseCandidate | 1.651 | 3.192 | +1.541 | 0.517× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 50 | 50 |
| Host entries | 50 | 50 |
| Host-to-guest bytes | 0 | 320000000 |
| Guest-to-host bytes | 0 | 320000000 |
| State round trips | 0 | 50 |
| Guest linear memory bytes | 6422528 | 6422528 |
| Peak RSS bytes | 58195968 | 71417856 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
