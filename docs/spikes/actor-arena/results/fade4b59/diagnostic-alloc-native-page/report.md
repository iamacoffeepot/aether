# Actor arena paired comparison

Base: `arena-endpoint`  
Candidate: `arena-page`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 2.220 ns/mail |
| Candidate median | 2.181 ns/mail |
| Median paired delta | -0.062 ns/mail |
| Paired delta IQR | 0.025 ns/mail |
| Relative median change | -1.77% |
| Median speedup | 1.029× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 1000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 2.220 | 2.158 | -0.062 | 1.029× |
| 1 | CandidateBase | 2.209 | 2.191 | -0.017 | 1.008× |
| 2 | BaseCandidate | 2.249 | 2.181 | -0.068 | 1.031× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 62500 | 62500 |
| State lock acquisitions | 62500 | 39709 |
| Scheduled items | 62500 | 39709 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 4390912 | 4456448 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
