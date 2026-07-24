# Actor arena paired comparison

Base: `wasm-arena`  
Candidate: `wasm-batch`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 17.819 ns/mail |
| Candidate median | 4.107 ns/mail |
| Median paired delta | -13.723 ns/mail |
| Paired delta IQR | 0.989 ns/mail |
| Relative median change | -76.95% |
| Median speedup | 4.388× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 1.782 ns/mail |

Configuration: `dispatch` workload, 1024 actors, 1000000 work units, 4096 bytes/state, 16 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 18.849 | 4.034 | -14.816 | 4.673× |
| 1 | CandidateBase | 17.433 | 4.119 | -13.315 | 4.233× |
| 2 | BaseCandidate | 17.752 | 4.051 | -13.701 | 4.382× |
| 3 | CandidateBase | 17.819 | 4.337 | -13.482 | 4.108× |
| 4 | BaseCandidate | 17.773 | 4.050 | -13.723 | 4.388× |
| 5 | CandidateBase | 17.723 | 4.188 | -13.535 | 4.232× |
| 6 | BaseCandidate | 18.016 | 4.100 | -13.916 | 4.394× |
| 7 | CandidateBase | 18.859 | 4.169 | -14.689 | 4.523× |
| 8 | BaseCandidate | 18.631 | 4.107 | -14.524 | 4.536× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 62500 | 977 |
| Host entries | 1000000 | 977 |
| Host-to-guest bytes | 8000000 | 16000000 |
| Guest linear memory bytes | 4259840 | 4259840 |
| Peak RSS bytes | 15269888 | 15646720 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
