# Actor arena paired comparison

Base: `wasm-detached`  
Candidate: `wasm-arena`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 20.562 ns/mail |
| Candidate median | 17.371 ns/mail |
| Median paired delta | -3.251 ns/mail |
| Paired delta IQR | 0.285 ns/mail |
| Relative median change | -15.52% |
| Median speedup | 1.189× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 2.056 ns/mail |

Configuration: `dispatch` workload, 512 actors, 100000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 20.435 | 17.183 | -3.251 | 1.189× |
| 1 | CandidateBase | 20.932 | 17.074 | -3.857 | 1.226× |
| 2 | BaseCandidate | 20.130 | 17.813 | -2.317 | 1.130× |
| 3 | CandidateBase | 20.942 | 17.465 | -3.477 | 1.199× |
| 4 | BaseCandidate | 20.562 | 17.371 | -3.191 | 1.184× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 100000 | 6250 |
| Host entries | 100000 | 100000 |
| Host-to-guest bytes | 800000 | 800000 |
| Guest linear memory bytes | 33554432 | 196608 |
| Peak RSS bytes | 22577152 | 10780672 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
