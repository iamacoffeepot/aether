# Actor arena paired comparison

Base: `arena-state`  
Candidate: `arena-endpoint`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 2.674 ns/mail |
| Candidate median | 2.271 ns/mail |
| Median paired delta | -0.396 ns/mail |
| Paired delta IQR | 0.024 ns/mail |
| Relative median change | -15.05% |
| Median speedup | 1.174× |
| Directional consistency | 88.9% |
| ADR-0085 noise floor | 0.300 ns/mail |

Configuration: `dispatch` workload, 4096 actors, 5000000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 2.678 | 2.282 | -0.396 | 1.174× |
| 1 | CandidateBase | 2.677 | 2.271 | -0.405 | 1.178× |
| 2 | BaseCandidate | 2.644 | 2.252 | -0.392 | 1.174× |
| 3 | CandidateBase | 2.674 | 2.293 | -0.381 | 1.166× |
| 4 | BaseCandidate | 2.687 | 2.252 | -0.436 | 1.193× |
| 5 | CandidateBase | 2.704 | 3.169 | +0.465 | 0.853× |
| 6 | BaseCandidate | 2.660 | 2.245 | -0.414 | 1.184× |
| 7 | CandidateBase | 2.633 | 2.228 | -0.405 | 1.182× |
| 8 | BaseCandidate | 2.611 | 2.412 | -0.199 | 1.082× |

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
| Peak RSS bytes | 6995968 | 6422528 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
