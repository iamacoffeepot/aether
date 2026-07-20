# Recovery

Recover from the symptom outward. Preserve evidence, prefer read-only checks,
and make the smallest mutation with clear ownership. A timeout, tool error, and
dead engine are different states; do not collapse them into “restart everything.”

## First response

For any unexpected result:

- [ ] Stop sending additional non-idempotent mail.
- [ ] Record the exact tool, arguments, selector/hash, and returned error.
- [ ] Call `list_engines(show: "alive")` with a fresh current hub view.
- [ ] If the engine is absent, call `list_engines(show: "dead")` immediately.
- [ ] If it is alive, record heartbeat age before further mutation.
- [ ] Collect reachable logs, descriptions, costs, traces, and frames.
- [ ] Decide who owns the engine/component before terminating or dropping it.

## Symptom-first matrix

| Symptom | First discriminating check | Safe next action |
|---|---|---|
| MCP tools are absent | is the tunnel up and MCP reconnected? | start the harness as documented, then reconnect the client |
| any old `engine_id` says “no supervised engine” | fresh `list_engines(show: "alive")` | reacquire after a hub restart; never substitute an RPC port |
| binary selector does not resolve | filtered `list_binaries` | `upload_binary`, then use returned hash/name |
| component selector does not resolve | registry `list_components` | `upload_component`, then use returned hash/name |
| spawn returns an allocated id in its error | `list_engines(show: "dead")` for that id | preserve matching `spawn_failed` detail |
| spawn boot readiness fails | diff live fleet against the pre-spawn snapshot | terminate only the attributable new engine; inspect reachable logs and substrate stderr |
| heartbeat age climbs | confirm heartbeat is enabled, then repeat one read-only check | stop mutations, collect evidence, terminate only if owned |
| engine disappears | recently-dead reason/detail | branch on terminated/crashed/evicted/spawn_failed |
| “unknown kind” or param encode error | explicit-engine `describe_kinds` for the exact name | correct schema, engine, or component load state |
| component name cannot be described | use full lineage returned by load | confirm engine/component still live; do not guess a short name |
| replica load fails partway | read failed index/count and derive the occupied name prefix | terminate an owned engine; stop and report if shared |
| `replace_component` errors | name-addressed `describe_component` plus a safe probe | observe current state, then roll forward by exact hash if needed |
| plain mail times out | live fleet, actor logs, externally visible state | do not resend until idempotence is established |
| traced mail times out | actor logs and host stderr | fix/reproduce only with a safe operation; timeout carries no trace handle |
| actor logs are unexpectedly empty | was the event inside a handler and admitted by filter? | inspect process stderr and logging configuration |
| frame capture fails or is empty | chassis manifest and `describe_handlers` | use a render-capable engine; inspect window/render state |

## Harness or hub interruption

The tunnel is the stable front. Follow [The MCP harness](../mcp-harness.md) to
start it and reconnect missing tools. Prefer a tunnel-admin hub restart when
rebuilding the hub; an `aether-mcp` restart invalidates the MCP session.
Restart only with explicit authority over the entire fleet, one coordinator,
and no blind retry; see [Harness lifecycle](harness-lifecycle.md).

The MCP RPC client re-dials a restarted hub on a dead connection with bounded
retry. A tool call crossing the restart can still fail and should be retried only
when it is read-only or known idempotent. Reconnection restores transport, not
the old fleet.

That retry can repeat the original envelope after an uncertain transport failure;
a lost reply is not proof the first mutation never landed. Reconcile observable
state before retrying spawn, lifecycle, or application mail.

After a hub restart:

- old engine ids are not continuity evidence, even if a fresh hub later reuses
  the same UUID text;
- old loaded component names/ids have no live engine behind them;
- the on-disk binary/component store is intended to remain;
- application state must be rebuilt from an application-owned persistence
  contract, not inferred from registry persistence.

Hub-only restart also preserves `aether-mcp` caches keyed by reusable engine
ids. If a clean kind/component view is essential, deliberately restart and
reconnect `aether-mcp`; otherwise treat cached descriptions as hints and prefer
live probes.

## Spawn failure

Separate pre-allocation from post-allocation failures.

If selector resolution fails, no engine id or port was allocated. Inspect
`list_binaries`; upload the chassis if necessary. Do not search the live fleet
for a child that was never created.

If an error includes an allocated `engine_id`, correlate it with the
`spawn_failed` recently-dead row. The proxy kills a child it could not bring up,
so the record is evidence, not a still-live engine handle.

Boot-component readiness is later: the proxy can be alive while one requested
lineage never registers. Keep a pre-spawn fleet snapshot. On a readiness error,
diff the current live set, inspect only attributable new engines, and terminate
the new id when ownership is unambiguous. In a busy shared fleet, ambiguity is a
reason to stop and ask—not to kill the newest-looking row.

Check common boot causes in this order:

1. component was uploaded before selector use;
2. selected module has the requested/default actor export;
3. config JSON matches the selected actor's Config schema;
4. derived replica/name lineages do not conflict;
5. substrate stderr reports wasm parse, compile, init, or registration failure.

## Live engine slow or gone

With hub heartbeats enabled, a rising age means pings are not being confirmed
promptly. It does not identify the stuck actor. If queries still land, collect
actor logs and cost rows from likely recipients. Avoid adding a traced workload
merely to diagnose a saturated engine unless the probe is harmless.

When the engine is absent, the death reason determines the next branch:

- `terminated`: verify that a known owner requested it. Unexpected termination
  is an ownership/co-ordination failure.
- `crashed`: preserve the detail and substrate/host stderr. Spawn a replacement
  only after capturing the failure evidence.
- `evicted`: the proxy crossed its heartbeat miss limit and dropped/killed the
  child. Investigate blocking, CPU starvation, or transport failure; the id is
  already dead.
- `spawn_failed`: return to selector/startup evidence rather than application
  mail debugging.

The death sidecar is bounded. Capture it before repeated failures push it out.

## Kind, mailbox, or component failure

For an unknown kind, call `describe_kinds` with the explicit current engine id
and the exact name. The encoder already refreshes once on a cache miss or stale
schema encode failure. A repeated miss usually means typo, wrong engine, or a
component that never loaded—not a cache that needs endless retries.

For an unresolved component recipient, use the exact lineage name returned by
load, not the artifact name, actor namespace, selector, or tagged id rendered as
a name. `describe_component` by lineage enables a live lookup on a cache miss;
pair a cache hit with a safe live probe when liveness is the question.

For partial replica loads, already-loaded instances stay live. The error reports
the failed index and how many succeeded, so their names follow the documented
`base-0` through prefix. It does not return the successful prefix's mailbox ids,
and registry `list_components` lists stored artifacts rather than those live
instances. The current MCP surface therefore cannot safely target that prefix
for drop unless the ids were obtained elsewhere.

If the engine belongs to the task, preserve evidence and terminate it. If the
engine is shared, stop and report the occupied name prefix to its owner. Do not
retry the same base or invent mailbox ids.

## Replacement failure

Replacement uses a stable mailbox binding on success, but recovery must trust
observation rather than intent. `drain_timeout_ms` is currently ignored, so
changing it cannot recover a stuck or failed splice. Pre-splice validation
errors preserve the old guest; later instantiation failure can leave the
trampoline empty, while rehydrate failure leaves the new guest installed even
though the operation returns `Err`.

The phase table and stale-introspection limits are in
[Replacement failure states](components/replacement-failure-states.md).

After an error:

- call `describe_component` by lineage, while treating a cache hit as a
  capability snapshot rather than conclusive liveness;
- issue a side-effect-free health/query mail if the component provides one;
- inspect its actor logs and, if useful, cost rows;
- do not drop the component until evidence is preserved;
- prefer a known-good exact content hash for a roll-forward attempt.

If the component is no longer serviceable and the engine is shared, drop that
exact mailbox and either refill the slot with `replace_component` or load a
differently named instance after checking downstream address ownership. If the
engine is task-owned, replacing the whole engine is often the cleaner isolation
boundary.

## Settlement timeout

A timeout is an observer deadline, not cancellation. The original engine chain
may still mutate state and eventually finish.

Plain `send_mail` preserves replies that arrived before its timeout. Save them,
especially recognized errors. Its later replies are no longer attached to a
pending MCP call. Inspect state and actor logs before considering a resend.

`send_mail_traced` timeout currently returns no root, replies, tree, or node
count. It therefore cannot be used to walk that timed-out chain later. If the
operation is safe to reproduce, first correct likely settlement obligations,
then reproduce with tracing. For a genuinely hung settlement, follow
[Debugging a hung settlement](../recipes/debugging-a-hung-settlement.md).

Never “fix” a timeout by switching to fire-and-forget unless the intended
contract is truly unobserved dispatch. That hides the symptom; it does not close
the causal chain.

## Missing logs or visual evidence

Actor rings contain only in-handler `tracing::*` events and are bounded. If the
expected line was emitted during boot, in a proxy, scheduler, panic hook, or
other host context, read process stderr. If `truncated_before` is present, state
the evidence gap explicitly.

For capture problems, confirm the selected binary manifest has the needed
chassis/caps, then query `describe_handlers`. Headless engines can run game logic
without a desktop frame. A backgrounded/minimized window may also require the
window-specific focus flow described in [Window](../systems/window.md).

Use a new `capture_frame.save_path` beneath a private task-owned evidence
directory when full-resolution bytes matter. Absolute syntax alone is not a
safety boundary. A failed host write is reported in the saved block without
invalidating the successful engine capture, so inspect both results. See
[Host evidence files](evidence/host-files.md).

## Final cleanup and handoff

- [ ] Preserve all returned spill-file and capture-file paths needed by the
      incident record.
- [ ] Drop task-owned components in a shared engine, require `drop_result`, and
      record the remaining empty slot.
- [ ] Terminate task-owned engines and verify the live fleet no longer lists
      them.
- [ ] Leave adopted/shared engines alone unless the owner authorizes action.
- [ ] Report stored artifact hashes separately from live engine/component ids.
- [ ] State evidence gaps: overwritten rings, sparse traced timeout, missing
      host stderr, or ambiguous fleet ownership.

## Source routes

- Recovery-visible tool contracts: `crates/aether-mcp/src/tools/mod.rs`
- Fleet outcomes: `crates/aether-mcp/src/tools/engine.rs` and
  `crates/aether-engine/src/server/runtime.rs`
- RPC reconnect and timeout behavior: `crates/aether-mcp/src/rpc.rs`
- Mail timeout projections: `crates/aether-mcp/src/tools/mail.rs`
- Live kind/component lookup: `crates/aether-mcp/src/tools/state.rs` and
  `describe.rs`
- Component lifecycle: `crates/aether-component/src/component/` and
  `crates/aether-component/src/trampoline/runtime/replace.rs`

Return to the [Operating overview](index.md) for the normal loop.
