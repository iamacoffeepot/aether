# Actor arena paired comparison

Base: `boxed-current`  
Candidate: `arena-state`  
Classification: **improvement**

| Metric | Result |
|---|---:|
| Base median | 40.061 ns/lifecycle op |
| Candidate median | 19.692 ns/lifecycle op |
| Median paired delta | -20.431 ns/lifecycle op |
| Paired delta IQR | 1.243 ns/lifecycle op |
| Relative median change | -50.85% |
| Median speedup | 2.038× |
| Directional consistency | 100.0% |
| ADR-0085 noise floor | 4.006 ns/mail |

Configuration: `lifecycle-churn` workload, 4096 actors, 250000 work units, 256 bytes/state, 1 mails/activation, 64 slots/page, `random` access, seed `6840227784451616781`.

## Pairs

| Pair | Order | Base ns/mail | Candidate ns/mail | Delta | Speedup |
|---:|---|---:|---:|---:|---:|
| 0 | BaseCandidate | 40.123 | 19.692 | -20.431 | 2.038× |
| 1 | CandidateBase | 39.058 | 20.825 | -18.233 | 1.876× |
| 2 | BaseCandidate | 40.061 | 19.341 | -20.720 | 2.071× |

## Mechanism counters

Counters are deterministic and shown from the first pair.

| Counter | Base | Candidate |
|---|---:|---:|
| Route lookups | 0 | 0 |
| State lock acquisitions | 0 | 0 |
| Scheduled items | 250000 | 250000 |
| Host entries | 0 | 0 |
| Host-to-guest bytes | 0 | 0 |
| Guest linear memory bytes | 0 | 0 |
| Peak RSS bytes | 6668288 | 5636096 |

## Interpretation limits

- Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.
- Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.
- Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.
- Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.
- Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.
- Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.
