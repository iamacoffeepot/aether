# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **inconclusive**

| Metric | Result |
|---|---:|
| Base median | 2.470 ns/mail |
| Candidate median | 2.753 ns/mail |
| Median paired delta | +0.277 ns/mail |
| Paired delta IQR | 0.016 ns/mail |
| Relative median change | +11.47% |
| Median speedup | 0.899× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 0.300 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 5000000 work units, 64 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 2.479 | 2.750 | +0.271 | 0.901× |
| 1 | CandidateBase | 2.492 | 2.753 | +0.261 | 0.905× |
| 2 | BaseCandidate | 2.468 | 2.755 | +0.287 | 0.896× |
| 3 | CandidateBase | 2.454 | 2.758 | +0.304 | 0.890× |
| 4 | BaseCandidate | 2.461 | 2.746 | +0.285 | 0.896× |
| 5 | CandidateBase | 2.488 | 2.764 | +0.276 | 0.900× |
| 6 | BaseCandidate | 2.470 | 2.776 | +0.306 | 0.890× |
| 7 | CandidateBase | 2.480 | 2.742 | +0.262 | 0.904× |
| 8 | BaseCandidate | 2.463 | 2.740 | +0.277 | 0.899× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 312500 | 312500 |
| State lock acquisitions | 312500 | 312500 |
| Scheduled items | 312500 | 312500 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 6356992 | 6193152 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
