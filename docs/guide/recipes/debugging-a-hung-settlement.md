# Debugging a hung settlement

**Class:** drive-only for triage; a source/SubstrateBench reproduction is usually
needed for root cause. Read [Tracing and settlement](../systems/tracing-and-settlement.md)
and [Concurrency and blocking](../systems/concurrency.md) for the model.

## Know the current observability limit

When `send_mail_traced` reaches its settlement timeout, the current MCP response
contains `status: "timeout"` and **no root, tree, node count, in-flight dump, or
reply list**. There is no standalone MCP tool that walks a still-live trace root.
The timeout also does not cancel work already dispatched.

That means a timeout alone cannot name the frontier actor. Any runbook that says
“inspect the partial tree returned on timeout” is describing a surface Aether
does not currently expose.

## 1. Preserve the operation identity

Record before doing anything else:

- engine id;
- exact recipient lineage;
- kind and parameters (redacting secrets/large bytes);
- whether the operation is safe to repeat;
- requested timeout and wall-clock duration;
- any application request id carried in the payload;
- whether the engine remained in `list_engines`.

Do not immediately reissue a paid provider call, write, spawn, or other
non-idempotent operation. The original may still be running and may later reply
or mutate state.

## 2. Check whether the engine is reachable

Call `list_engines(show: "alive")`, then a narrow read-only query such as
`describe_handlers` or `actor_logs` for a known actor.

| Observation | Interpretation |
|---|---|
| Engine gone/recently crashed or evicted | process/heartbeat failure, not merely one settlement chain |
| Engine live but all queries stall | dispatcher/process pressure or broader wedge |
| Engine live and other actors answer | local chain/actor/offload problem |

Capture the matching recently-dead detail promptly; it is a bounded sidecar, not
a durable audit log.

## 3. Read logs at known boundaries

Start with the original recipient:

```text
actor_logs(engine_id, mailbox_name = "<exact recipient>")
```

Then inspect only downstream actors you can establish from the contract or log
evidence. Useful signs include:

- blocking work submitted but no task completion;
- a queue at its concurrency bound;
- a response/stream opened but never ended;
- an actor waiting for a reply from a missing or dropped component;
- a panic, decode failure, or monitor notice;
- repeated “slow gate”/hold diagnostics.

Only in-actor tracing reaches actor rings. Host thread panics and process
startup failures may exist only in captured process/CI stderr.

## 4. Check cost and pressure

`actor_cost` can distinguish a handler that completes slowly from one that has
not produced samples or whose work happens off-dispatcher. Look for:

- unusually high handler mean/MAD;
- one hot actor serializing unrelated work behind it;
- expected completion handler with no samples;
- fan-out/queue pressure consistent with the elapsed time.

Cost is evidence, not a settlement table. A cheap initiating handler may have
launched blocked work elsewhere.

## 5. Classify likely causes

### Hold acquired but never released

An asynchronous path acquired or inherited a settlement hold, then lost the
completion/release path. Common seams are error returns, cancelled queue items,
shutdown, and dropped reply targets. Audit every branch after acquisition.

Prefer sanctioned primitives (`dispatch_blocking` with task completion,
inherited worker helpers, or scoped hold guards) over raw threads and hand-kept
counters.

### Blocking work ran on the dispatcher

A handler performed socket/file/provider waiting, slept, waited on a contended
lock/channel, or joined a thread inline. One actor can then pin a worker and
queue descendants needed for its own completion.

Move host waiting to the established sidecar/task primitive while keeping
mutable state transitions on the actor.

### Completion cannot route back

The worker finished but its task/completion mail targets a dropped or wrong
instance, uses stale lineage after a hub/engine restart, or lost correlation.
Check the exact engine epoch and loaded component names.

### Stream or drain never terminated

Long-lived HTTP/TCP/callback paths must distinguish the opening request chain
from detached data-phase work. Missing end/close events, over-credit teardown,
or a drain that failed to discharge an owned dispatch can strand obligations.

### Merely slow work

Provider calls and large fan-out can legitimately approach the tool timeout.
Use logs, application request ids, cost, and a controlled reproduction to prove
progress. Raising the timeout (up to the live schema's cap) is appropriate only
after the operation's own timeout and retry semantics are understood.

## 6. Reproduce where pending state is visible

For a code-level bug, create the smallest focused SubstrateBench case. Its settlement
timeout includes a pending-root dump with in-flight/held-open counts, which is
more actionable than the current MCP timeout response. Pin the settlement cap
low enough for the test while leaving normal scheduler contention room.

A good regression test:

1. sends one typed root;
2. crosses the suspected offload/queue/drain seam;
3. waits through the normal settlement gate;
4. asserts the result/side effect, not elapsed sleep;
5. fails with the pending-root evidence if the chain wedges.

Use FleetBench only when the suspected cause requires the hub/RPC/process
boundary. A pure hold or actor queue bug is easier to localize in SubstrateBench.

## 7. Verify the fix

- The original causal operation settles without arbitrary sleeps.
- Error, cancellation, shutdown, and success paths all release/transfer holds.
- A late worker cannot reply into a replaced/dropped owner incorrectly.
- The regression fails on the old behavior and passes on the fix.
- Other engine operations stay responsive under the same load.
- Logs do not contain secrets or unbounded payloads.

## If you need a missing tool

A partial in-flight trace query would materially improve live diagnosis, but it
does not exist today. Add it as an explicit, bounded MCP/trace-capability change
with a clear root-discovery and ring-truncation contract; do not write prose as
though it already shipped.

Continue with [Inspection and debugging](../operating/inspect-and-debug.md) for
the broader symptom tree and [SubstrateBench and FleetBench](../testing/substratebench-and-fleetbench.md)
for reproduction boundaries.
