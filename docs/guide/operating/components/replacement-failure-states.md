# Replacement failure states

A `publish` of a successor republishes a module over every live instance of its
namespaces as one group (ADR-0241 §7): on success every instance runs the
successor behind its unchanged mailbox, and on failure every instance runs its
old guest. An error is still not a clean rollback signal. Depending on which
phase failed, an old guest may already have run its `unwire` and `on_dehydrate`
hooks. It gets back the state its `on_dehydrate` saved, through its
`on_rehydrate`, and runs `wire` again; only teardown outside that saved state
and outside what `wire` rebuilds stays gone. Nothing a successor sent leaves: its mail is held
until the group commits, and a failed successor's mail is discarded.
Introspection can also describe a retained capability snapshot rather than the
state an installed guest is actually in.

Read [Component registry](../component-registry.md) first for normal publish,
spawn, load, and drop behavior.

## Phase-dependent residue

| Failure phase | Guests left behind | Capability/introspection risk |
|---|---|---|
| a pre-check: bad wasm, no predecessor, content-addressed, a republish in flight, a boot, a dropped namespace or narrowed contract (ADR-0231 §5), an unmet added dependency, or a missing or undecodable config | every instance is untouched; no hook ran | existing descriptions still reflect the prior registry snapshot |
| a successor's `init` fails in one instance | every instance reinstates its old guest; an instance whose own prepare succeeded ran its unwire/dehydrate hooks, gets its saved state back through `on_rehydrate`, and runs its `wire` again | old descriptions remain the best snapshot, but those guests may have changed lifecycle state their saved state does not carry |
| `save_state` host-call rejection, a live held reply left unsaved (ADR-0243 §6), a carried request context the successor does not declare (ADR-0139 §4), or a failed rehydrate in one instance | every instance reinstates its old guest with its pending replies, counters, and the state its `on_dehydrate` saved (none after a rejected `save_state`), and runs its `wire` again; nothing a successor sent from `init` or `on_rehydrate` leaves | as above |
| the module publish is refused (admission against the table as the owner stages it) | every instance reinstates its old guest with the state it saved and runs its `wire` again | as above |
| success | every instance runs its successor and the new capabilities are registered | MCP refreshes its cached instances of each republished type from the result |

The exact phase matters more than the generic `Err` shape. Do not say
“replacement rolled back” unless a current behavioral observation proves the
old guests still serve their mailboxes.

An `unwire` or `on_dehydrate` guest trap is different: those traps are logged
and contained rather than returned as `PublishResult::Err`, and the prepare
continues. Only a rejected `save_state` host call is surfaced at that phase. If
the group later commits, do not mistake an earlier hook-trap log for a
rolled-back swap.

## Why `describe_component` can mislead

There are two description layers:

1. `aether-mcp` returns a process-local cache hit immediately. It refreshes that
   cache on a successful load or publish, but retains the prior entries on a
   publish error.
2. On a cache miss addressed by lineage name, the substrate returns its
   capability registry entry. Some post-splice failures happen before that
   entry is removed or replaced.

After a failed replacement, restarting only `aether-mcp` can bypass its cache
and still obtain a stale substrate registry entry. Kind presence and cost rows
can likewise outlive or disagree with the currently installed guest. Treat all
of them as snapshots, not liveness or binary-identity proof.

## Recovery protocol

After any publish error:

1. Stop further lifecycle mutation on the module's instances.
2. Record the exact selector/hash, configs, the instances the error names, the
   error, logs, and pre-replace capabilities.
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
the current state and the mailbox is still safe to mutate. There is no drain
knob to reach for: the splice is structural, so no argument slows or retries it.

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

Even after `PublishResult::Ok`:

- retain the exact content hash used;
- compare each returned type's capabilities with the expected one;
- run a harmless live probe;
- confirm downstream senders still address the stable lineage;
- treat a mutable registry name only as provenance context, not selected-byte
  identity.

## Source routes

- Group pre-checks, prepare, publish, commit and abort:
  `crates/aether-component/src/component/runtime/republish/`
- One instance's prepare, commit and abort:
  `crates/aether-component/src/trampoline/runtime/republish.rs`
- Substrate live component description:
  `crates/aether-component/src/component/runtime/mod.rs`
- MCP publish result and cache update:
  `crates/aether-mcp/src/tools/components/publish.rs`
- MCP description cache and live fallback:
  `crates/aether-mcp/src/tools/describe.rs`
- Live kind cache contract:
  [ADR-0091](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0091-live-kind-schemas-on-the-inventory-cap.md)
