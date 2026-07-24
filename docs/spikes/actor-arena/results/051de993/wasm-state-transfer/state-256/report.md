# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 2.347 ns/entity update |
| Candidate median | 9.564 ns/entity update |
| Median paired delta | +7.191 ns/entity update |
| Paired delta IQR | 0.250 ns/entity update |
| Relative median change | +307.54% |
| Median speedup | 0.248× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.376 ns/entity update |

Configuration: `scene-sweep` workload, 65536 actors, 2097152 work units, 256 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 2.298 | 9.120 | +6.822 | 0.252× |
| 1 | CandidateBase | 2.576 | 10.003 | +7.427 | 0.258× |
| 2 | BaseCandidate | 2.420 | 9.560 | +7.140 | 0.253× |
| 3 | CandidateBase | 2.373 | 9.564 | +7.191 | 0.248× |
| 4 | BaseCandidate | 2.347 | 9.465 | +7.119 | 0.248× |
| 5 | CandidateBase | 2.293 | 9.625 | +7.333 | 0.238× |
| 6 | BaseCandidate | 2.318 | 9.894 | +7.576 | 0.234× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 32 | 32 |
| Host entries | 32 | 32 |
| Host-to-guest bytes | 0 | 536870912 |
| Guest-to-host bytes | 0 | 536870912 |
| State round trips | 0 | 32 |
| Guest linear memory bytes | 16842752 | 16842752 |
| Peak RSS bytes | 45056000 | 79151104 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
