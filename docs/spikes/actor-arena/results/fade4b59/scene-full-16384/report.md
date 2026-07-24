# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-page`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 6.749 ns/entity update |
| Candidate median | 2.936 ns/entity update |
| Median paired delta | -3.737 ns/entity update |
| Paired delta IQR | 0.110 ns/entity update |
| Relative median change | -56.49% |
| Median speedup | 2.281× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.675 ns/mail |

Configuration: `scene-sweep` workload, 16384 actors, 5000000 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 6.429 | 2.816 | -3.613 | 2.283× |
| 1 | CandidateBase | 6.549 | 2.872 | -3.677 | 2.281× |
| 2 | BaseCandidate | 6.648 | 2.911 | -3.737 | 2.284× |
| 3 | CandidateBase | 6.765 | 2.979 | -3.786 | 2.271× |
| 4 | BaseCandidate | 6.749 | 3.073 | -3.676 | 2.196× |
| 5 | CandidateBase | 6.817 | 2.929 | -3.888 | 2.327× |
| 6 | BaseCandidate | 6.751 | 2.995 | -3.757 | 2.254× |
| 7 | CandidateBase | 6.814 | 2.957 | -3.857 | 2.304× |
| 8 | BaseCandidate | 6.502 | 2.936 | -3.565 | 2.214× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 5000000 | 78125 |
| Scheduled items | 5000000 | 78125 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 47529984 | 45039616 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
