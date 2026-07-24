# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-copy-roundtrip`  
Classification: **regression**

| Metric | Result |
|---|---:|
| Base median | 5.524 ns/entity update |
| Candidate median | 155.285 ns/entity update |
| Median paired delta | +149.761 ns/entity update |
| Paired delta IQR | 23.708 ns/entity update |
| Relative median change | +2710.94% |
| Median speedup | 0.036× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 35.562 ns/entity update |

Configuration: `scene-sweep` workload, 16384 actors, 131072 work units, 4096 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/entity update | Candidate ns/entity update | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 5.196 | 131.254 | +126.058 | 0.040× |
| 1 | CandidateBase | 5.221 | 131.927 | +126.705 | 0.040× |
| 2 | BaseCandidate | 5.648 | 161.791 | +156.143 | 0.035× |
| 3 | CandidateBase | 5.624 | 156.037 | +150.413 | 0.036× |
| 4 | BaseCandidate | 5.524 | 155.285 | +149.761 | 0.036× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 8 | 8 |
| Host entries | 8 | 8 |
| Host-to-guest bytes | 0 | 536870912 |
| Guest-to-host bytes | 0 | 536870912 |
| State round trips | 0 | 8 |
| Guest linear memory bytes | 67174400 | 67174400 |
| Peak RSS bytes | 79659008 | 214007808 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
