# Session-pool spike (#3264)

Measure-then-experiment spike for reusing Claude sessions and cached prefixes across fleet
boxes. Two halves: a ledger pass over the fleet's own exhaust, and live resume experiments
that validate the warm-pool economics before anything is built.

## Half 1 — the ledger (no new spend)

`extract.py` walks every unexpired session-transcript artifact (agent-work, review, dogfood,
judge) and emits one row per box into `data/boxes.jsonl`: the final `result` usage aggregate
plus the first main-model API call's usage. On a fresh ephemeral runner, a first-call cache
read can only be a cross-process server-side hit, so that field alone answers the issue's
"do separate `claude -p` processes share the cache?" question.

Headline numbers (322 boxes, 2026-07-10 → 2026-07-13; full tables in the issue comment):

- 322/322 boxes hit cache on their first call (median 28,653 tokens read) — cross-box
  prefix caching already works, and 100% of writes are in the `ephemeral_1h` bucket.
- Each box then re-writes a median 26,544 tokens: the CLAUDE.md system-reminder and the
  task text share the first user message, so the cacheable prefix ends at the system
  prompt. The 46 boxes with a zero-write first call are exactly the byte-identical
  invocations (approve-sweep's constant ref, same-ref re-dispatches).
- Discovery dominates: 81–100% of a box's cache writes (by task) land before its first
  mutating action. The accumulated context is overwhelmingly re-derivable exploration —
  the warm-pool prize.

## Half 2 — resume experiments

Playground: six real doc files, sonnet, a deposit session prompted with a standing reuse
notice ("this session may be resumed by a different job for a different task; your
knowledge of these files is the asset").

| experiment | first call | outcome |
|---|---|---|
| deposit (10 turns, discovery) | — | 92K context written, $0.60 |
| warm resume, new task, new process, session store round-tripped through a stand-in S3 | read 91,918 / wrote 302 | 2 turns, $0.098, zero re-reads |
| cold control, same task | — | 5 turns, $0.304 |
| opus resume of the sonnet session | read 15,166 / wrote 64,651 | $0.66 for one sentence — the guaranteed-miss inversion |
| past-TTL resume (65 min later) | recorded on #3264 | validates the age bound |

Findings beyond the issue's settled invariants:

- **Tool config is part of the key.** The tools block is prefix bytes; changing
  `allowedTools` or MCP config between deposit and resume breaks the hit even same-model.
- **Pin the CLI version fleet-wide.** An unpinned `npm install -g` means a Claude Code
  release mid-window changes the system-prompt bytes and cold-misses the entire pool.
- **Key on the box's own checkout, not deposit-time re-hashing.** Re-hashing files when
  the manifest is written races with concurrent mutations; `git rev-parse HEAD:<path>`
  blob hashes from the box's checkout at exit are race-free.
- **The reuse notice works.** The resumed session treated its history as subsystem
  knowledge, not a task continuation — clean pickup, no contamination.

## Prototype

`pool.py` — the session database in ~130 lines: `deposit` derives a manifest from what the
session actually read (session id, model, tool fingerprint, file set + tree hash, context
size — all recoverable from the stream-json transcript the workflows already tee);
`checkout` enforces the settled invariants (model key, age bound with delete-not-keep,
tree-hash invalidation, exclusive expiring lease with self-healing reclaim); `release`
drops the lease. Every invariant was exercised live, including lease contention and the
stale-tree retirement.
