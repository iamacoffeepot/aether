# Inspect and debug

Debug a live engine from the outside in: prove the fleet target, learn its live
surface, issue the smallest safe probe, then gather actor-local evidence. This
order prevents a schema guess or stale engine id from masquerading as an
application failure.

## Observation map

| Question | Current tool | Scope | Important limit |
|---|---|---|---|
| Is the substrate supervised? | `list_engines` | hub | when enabled, heartbeat is proxy liveness, not handler health |
| Which kinds exist and what are their fields? | `describe_kinds` | one engine when selected | no explicit id with zero/many engines yields static baseline |
| What do native caps accept and reply with? | `describe_handlers` | one engine | component-defined reply names may remain unresolved |
| What does this wasm actor advertise? | `describe_component` | one loaded component | cache-first; lineage name enables live fallback on a miss |
| Which pure native transforms are linked? | `describe_transforms` | MCP build-static | not per-engine runtime state |
| What did one actor say? | `actor_logs` | one mailbox | bounded ring; in-handler events only |
| What does one handler cost? | `actor_cost` | one mailbox | EWMA instrumentation, not a profiler or scheduler control |
| Did mail settle and what replied? | `send_mail` | one or more independent items | fixed await bound; partial replies on timeout |
| What causal chain ran? | `send_mail_traced` | one atomic batch/engine | trace rings are best-effort and bounded |
| What is rendered now? | `capture_frame` | one engine | requires a render-capable chassis |

## 1. Pin the target

Start with `list_engines(show: "alive")`. Record the `engine_id`, RPC port, and
heartbeat age together. If the engine id came from an earlier hub lifetime,
discard it and select from the current list.

In a multi-engine fleet, pass `engine_id` explicitly to every engine-scoped
inspection. In particular, a bare `describe_kinds` only auto-selects when
exactly one engine is supervised. With zero or several engines it intentionally
falls back to the static substrate vocabulary, which omits the live component
set and must not be presented as that engine's inventory.

## 2. Learn the live contract

Use the narrowest introspection call that answers the question:

- Start `describe_kinds` with `families: true` for a digest.
- Use `prefix` for one namespace or `names` for exact kinds.
- Add `full: true` only after selecting `prefix` or `names`; bare full output is
  rejected to keep responses bounded.
- Use `describe_handlers` for native mailbox request → reply contracts.
- Use `describe_component` for a wasm component's handlers, reply contracts,
  fallback, docs, and Config kind.

`describe_kinds` merges the MCP static baseline with the substrate's live
`aether.inventory.kinds` response. The substrate serves its shared registry, so
component-defined kinds become visible as soon as loading registers them.
The refresh is best-effort and merged into the existing per-engine cache; a
failed live query leaves prior data rather than marking it unavailable. Use a
kind description to construct mail, not as sole proof that an actor is live.

`describe_component` first checks a process-local capability cache. On a miss,
a lineage name resolves live against the substrate. Keep the exact name returned
by load or boot discovery; it enables that fallback. A tagged mailbox id works
only when the local MCP cache already has that `(engine, mailbox)` entry.

`load_component` and `replace_component` populate the cache. A generic component
drop through `send_mail` does not invalidate it, so a later same-process describe
can show stale pre-drop capabilities. Likewise, component kind descriptors
remain in the engine registry after drop. Neither observation alone proves the
guest is still loaded.

Hub restarts do not clear these MCP caches, while the new hub can reuse an
`engine_id`. Begin a new hub epoch with fleet discovery and live probes. Restart
and reconnect `aether-mcp` only when a clean cache is required and session
invalidation is acceptable.

## 3. Probe with settlement

Use `send_mail` for independent calls. Each item is best-effort relative to its
siblings: one item can error without aborting the rest. By default an item waits
for its causal chain to settle and returns the terminal reply plus recognized
errors. Choose `replies: "all"` when an event stream matters and `"none"` when
only recognized failures are useful.

Use `send_mail_traced` when any of these are true:

- every spec must validate before any of the batch moves;
- the whole batch needs one shared causal root;
- handler and queue timing matter;
- settlement itself is under investigation.

The default traced projection is a compact, indented `tree`. `full: true`
returns nodes and parent edges instead. There is no separate public MCP
`trace_tail` tool; `send_mail_traced` performs the guided ring walk internally.

Do not use `fire_and_forget` as a health check. `dispatched` confirms the call
was written or its immediate traced ack arrived; it does not prove downstream
work completed.

## 4. Read actor-local evidence

`actor_logs` queries exactly one mailbox. Use `level` and `contains` to filter
inside the actor, and thread `next_since` into the next call's `since` so a poll
does not duplicate entries. Preserve `truncated_before`: it proves the bounded
ring evicted unseen entries.

Only `tracing::*` emitted while an actor handler is dispatched enters that
actor's ring. Substrate boot, scheduler, proxy, panic-hook, and other host-side
events go to process stderr instead. An empty ring is therefore not proof that
the host emitted nothing. See [Logging](../systems/logging.md).

`actor_cost` reads one actor's per-handler execution-cost EWMA. A row gives mean
and mean absolute deviation in nanoseconds plus a sample count. It is
measure-only: reading it does not change scheduling, and an EWMA is not a full
distribution. A zero sample count is a seeded handler, not observed work.

## 5. Capture visual evidence

`capture_frame` can return an inline PNG, execute substrate-side checks, compare
a reference image, and/or persist the original full-resolution PNG with an
absolute `save_path`. Checks operate on full-resolution RGBA even when the
inline image is reduced or omitted.

Mail in `mails` runs atomically before readback; `after_mails` runs after it.
That makes capture capable of engine mutation. Empty bundles make it an
engine-state snapshot, but `save_path` still creates parent directories and can
overwrite a host file. Omit the bundles **and** `save_path` for a fully
non-mutating observation. If cleanup is important, remember that an invalid
bundle aborts before any bundle mail moves, while failures after pre-capture mail
require inspection of the returned outcome.

Headless/windowless engines cannot produce a desktop frame. Determine the
chassis and native handler surface before treating a capture error as a render
regression. Rendering semantics and window focus behavior are covered in
[Rendering & camera](../systems/rendering.md) and [Window](../systems/window.md).

## Timeout and observability limits

Timeouts bound the observer; they do not cancel engine work.

| Surface | Current bound behavior | What remains observable |
|---|---|---|
| `send_mail` | fixed 300-second whole-call await per item | status `timeout`, `timed_out: true`, and projected replies collected so far |
| `send_mail_traced` | defaults to 300 seconds; caller value clamps at 600 seconds | timeout response omits root, replies, tree, and node count |
| `fire_and_forget` mail | no settlement wait | later state/log/frame evidence only |
| most `call_one`-based tools | no shared MCP settlement deadline | transport close/error or the tool's own subsystem bound |

These are await bounds, not whole-tool deadlines. Plain `send_mail` starts its
bound after schema lookup/encoding. For traced mail,
`settlement_timeout_ms` bounds the initial ack/settlement collection; after a
successful settle, the guided per-actor trace walk uses ordinary `call_one`
queries and is not covered by that value.

When a plain mail times out, its MCP pending entry is removed and later replies
are not recoverable through that call. Do not blindly resend a non-idempotent
operation: the original chain may still finish. Observe state and logs first.

A traced timeout is especially sparse by current contract. It is evidence that
the harness stopped waiting before settlement, not a retained trace handle.
Reproduce with tracing only when repeating the operation is safe.

Settlement is exact when it reports success, but trace storage is best-effort.
Actor trace rings can overwrite old or high-volume data, so a settled tree may
be incomplete or unavailable if read too late. Read
[Tracing & settlement](../systems/tracing-and-settlement.md) for the distinction.

## Evidence bundle checklist

Before terminate, restart, replace, or retry, capture what applies:

- [ ] current `list_engines(show: "alive")` row;
- [ ] current `list_engines(show: "dead")` row if the engine vanished;
- [ ] exact binary/component content hash and selector used;
- [ ] narrow `describe_kinds` output for request and reply kinds;
- [ ] `describe_handlers` or `describe_component` output;
- [ ] `actor_logs` result with cursor and truncation fields;
- [ ] compact or full `send_mail_traced` result, if reproduction was safe;
- [ ] `actor_cost` rows for a slow handler;
- [ ] capture verdict and a full-resolution `save_path`, when visual;
- [ ] host/tunnel/substrate stderr for events outside actor handlers.

Large decoded byte leaves and oversized complete tool responses can be spilled
to harness-host temporary files rather than truncated. Treat a returned `file`
path as part of the evidence: read or copy it while it still exists, and clean
it up when retention is no longer needed.

## Source routes

- Tool inventory and public descriptions: `crates/aether-mcp/src/tools/mod.rs`
- Argument projections and timeout response shapes:
  `crates/aether-mcp/src/args.rs`
- Live descriptions: `crates/aether-mcp/src/tools/describe.rs` and `state.rs`
- Settlement/timeouts: `crates/aether-mcp/src/tools/mail.rs` and
  `crates/aether-mcp/src/rpc.rs`
- Logs and cost projection: `crates/aether-mcp/src/tools/logs_cost.rs`
- Capture behavior: `crates/aether-mcp/src/tools/capture.rs`
- Substrate inventory implementation: `crates/aether-capabilities/src/inventory/`

For a failure already in progress, continue with [Recovery](recovery.md).
