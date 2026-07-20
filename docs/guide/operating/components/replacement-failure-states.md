# Replacement failure states

`replace_component` preserves a trampoline mailbox on success, but an error is
not a universal rollback signal. The old guest, an empty slot, or the new guest
can remain depending on which phase failed. Introspection can also describe a
retained capability snapshot rather than the guest that is actually installed.

Read [Component registry](../component-registry.md) first for normal load,
replace, and drop behavior.

## Phase-dependent residue

| Failure phase | Guest left behind | Capability/introspection risk |
|---|---|---|
| new wasm compile, manifest parse, or export selection | prior slot is unchanged: old guest or an already-empty post-drop slot | existing descriptions still reflect the prior registry snapshot, not a new guest |
| `save_state` host-call rejection during dehydrate | old guest object is restored after its unwire/dehydrate hooks already ran | old description remains the best snapshot, but the guest may have changed its own lifecycle state |
| new guest instantiation failure after old guest was taken | trampoline can be empty | old capability registry and MCP cache can remain even though no guest is live |
| new guest rehydrate failure | new guest remains installed despite `Err` | capability re-registration has not run; old substrate and MCP descriptions can remain |
| successful replacement | new guest remains and new capabilities are registered | MCP refreshes its cache from the success result |

The exact phase matters more than the generic `Err` shape. Do not say
“replacement rolled back” unless a current behavioral observation proves the
old guest still serves the mailbox.

An `unwire` or `on_dehydrate` guest trap is different: those traps are logged
and contained rather than returned as `ReplaceResult::Err`, and replacement
continues. Only a rejected `save_state` host call is surfaced at that phase. If
the later replacement succeeds, do not mistake an earlier hook-trap log for a
rolled-back splice.

## Why `describe_component` can mislead

There are two description layers:

1. `aether-mcp` returns a process-local cache hit immediately. It refreshes that
   cache on successful load/replace, but retains the prior entry on replace
   error.
2. On a cache miss addressed by lineage name, the substrate returns its
   capability registry entry. Some post-splice failures happen before that
   entry is removed or replaced.

After a failed replacement, restarting only `aether-mcp` can bypass its cache
and still obtain a stale substrate registry entry. Kind presence and cost rows
can likewise outlive or disagree with the currently installed guest. Treat all
of them as snapshots, not liveness or binary-identity proof.

## Recovery protocol

After any replace error:

1. Stop further lifecycle mutation on that mailbox.
2. Record the exact selector/hash, export, config, mailbox id, error, logs, and
   pre-replace capabilities.
3. Call `describe_component` by lineage only as supporting evidence; label a
   cache hit or live-registry reply as potentially stale.
4. Use one known side-effect-free application query that distinguishes the old
   and new build when such a query exists.
5. If no safe discriminating probe exists, classify the slot as indeterminate.
6. On a task-owned engine, prefer terminating and recreating the engine from
   known hashes over repeated splice attempts.
7. On a shared engine, stop and report the indeterminate mailbox to its owner.
   Do not drop, refill, or retry by guess.

A roll-forward by known-good hash is appropriate only after the owner accepts
the current state and the mailbox is still safe to mutate. `drain_timeout_ms`
is currently ignored and cannot repair a failed splice.

## Designing a discriminating probe

A useful probe:

- is documented as read-only or idempotent;
- has a bounded reply;
- is handled differently by the candidate builds or proves required state;
- does not depend on a kind that may itself be stale only in the MCP encode
  cache;
- records its exact request and reply as evidence.

If the only available operation mutates application state, do not use it merely
to answer “which guest is installed?” Recreate an owned engine or escalate a
shared one instead.

## Success verification

Even after `ReplaceResult::Ok`:

- retain the exact content hash used;
- compare returned capabilities with the expected actor export;
- run a harmless live probe;
- confirm downstream senders still address the stable lineage;
- treat a mutable registry name only as provenance context, not selected-byte
  identity.

## Source routes

- Phase transitions and capability registration:
  `crates/aether-component/src/trampoline/runtime/replace.rs`
- Substrate live component description:
  `crates/aether-component/src/component/runtime/mod.rs`
- MCP replace result and cache update:
  `crates/aether-mcp/src/tools/components.rs`
- MCP description cache and live fallback:
  `crates/aether-mcp/src/tools/describe.rs`
- Live kind cache contract:
  [ADR-0091](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0091-live-kind-schemas-on-the-inventory-cap.md)
