# Actor arena paired comparison

Base: `wasm-detached`  
Candidate: `wasm-arena`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 19.348 ns/mail |
| Candidate median | 16.984 ns/mail |
| Median paired delta | -2.332 ns/mail |
| Paired delta IQR | 0.453 ns/mail |
| Relative median change | -12.22% |
| Median speedup | 1.137× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 1.935 ns/mail |

Configuration: `dispatch` workload, 128 actors, 100000 work units, 256 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 19.372 | 16.826 | -2.546 | 1.151× |
| 1 | CandidateBase | 18.450 | 16.742 | -1.708 | 1.102× |
| 2 | BaseCandidate | 19.348 | 17.016 | -2.332 | 1.137× |
| 3 | CandidateBase | 19.470 | 17.003 | -2.467 | 1.145× |
| 4 | BaseCandidate | 18.998 | 16.984 | -2.014 | 1.119× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 100000 | 6250 |
| Host entries | 100000 | 100000 |
| Host-to-guest bytes | 800000 | 800000 |
| Guest linear memory bytes | 8388608 | 65536 |
| Peak RSS bytes | 13631488 | 10354688 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
