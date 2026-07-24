# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 3.832 ns/entity update |
| Candidate median | 35.742 ns/entity update |
| Median paired delta | +31.994 ns/entity update |
| Paired delta IQR | 1.623 ns/entity update |
| Relative median change | +832.65% |
| Median speedup | 0.105× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 2.435 ns/entity update |

Configuration: `scene-sweep` workload, 32768 actors, 524288 work units, 1024 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 3.677 | 35.196 | +31.519 | 0.104× |
| 1 | CandidateBase | 3.635 | 34.749 | +31.114 | 0.105× |
| 2 | BaseCandidate | 3.748 | 35.742 | +31.994 | 0.105× |
| 3 | CandidateBase | 3.834 | 36.692 | +32.858 | 0.104× |
| 4 | BaseCandidate | 3.832 | 35.619 | +31.787 | 0.108× |
| 5 | CandidateBase | 4.090 | 37.785 | +33.695 | 0.108× |
| 6 | BaseCandidate | 4.043 | 40.569 | +36.527 | 0.100× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 16 | 16 |
| Host entries | 16 | 16 |
| Host-to-guest bytes | 0 | 536870912 |
| Guest-to-host bytes | 0 | 536870912 |
| State round trips | 0 | 16 |
| Guest linear memory bytes | 33619968 | 33619968 |
| Peak RSS bytes | 49364992 | 116621312 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
