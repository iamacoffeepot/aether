# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-page`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 6.453 ns/entity update |
| Candidate median | 2.896 ns/entity update |
| Median paired delta | -3.559 ns/entity update |
| Paired delta IQR | 0.075 ns/entity update |
| Relative median change | -55.12% |
| Median speedup | 2.222× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.645 ns/mail |

Configuration: `scene-sweep` workload, 4096 actors, 5000000 work units, 64 bytes/state, 1 mails/activation, 64 slots/page, `sequential` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 6.449 | 2.905 | -3.545 | 2.220× |
| 1 | CandidateBase | 6.550 | 2.846 | -3.704 | 2.301× |
| 2 | BaseCandidate | 6.429 | 2.815 | -3.615 | 2.284× |
| 3 | CandidateBase | 6.435 | 2.896 | -3.539 | 2.222× |
| 4 | BaseCandidate | 6.399 | 2.818 | -3.581 | 2.271× |
| 5 | CandidateBase | 6.453 | 2.933 | -3.520 | 2.200× |
| 6 | BaseCandidate | 6.562 | 3.067 | -3.496 | 2.140× |
| 7 | CandidateBase | 6.673 | 3.114 | -3.559 | 2.143× |
| 8 | BaseCandidate | 6.487 | 2.769 | -3.718 | 2.343× |

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
| Peak RSS bytes | 43859968 | 43220992 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
