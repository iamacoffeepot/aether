# Kernel reactor: design for review

- **Status:** Owner approved for implementation; PR review pending
- **Date:** 2026-09-17
- **Grounding:** `78493530dc4bbe2cc0c434516c345dc9cf7a319a`
- **Decision:** [ADR-0223](../adr/0223-journal-selected-kernel-reactor.md)
- **Plans:** [Work orders](bloomery-kernel-work-orders.md)

## 1. Agreed behavior

The kernel is a WASM reactor bundle with its views compiled into it. It is
always required while the application runs and can be replaced. Genesis is
just a pinned initial build: store its bytes through the ordinary artifact
path. No special genesis format, WASM-pack wrapper, or kernel upgrade protocol
is required.

Reactors respond to what they observe. Lifecycle success and failure must
therefore be observable events, not only native logs. External agents and
humans may react to those events; automatic recovery policy is not required.

A component replacement is transactional. Success installs the successor.
Rejection preserves the predecessor's usable state, mailbox, peers, and
bindings. It must not leak partial lifecycle effects. Individual kernel drop
is prohibited by host-owned flags. Whole-application shutdown remains valid.

## 2. Journal-selected code

Existing API with proposed conventional names:

```rust
const KERNEL: Head<OpaqueBytes> = Head::new("core.kernel");

struct ReactorSet {
    clusters: BTreeSet<Head<OpaqueBytes>>,
}

const REACTORS: Head<ReactorSet> = Head::new("core.reactors");
```

`ReactorSet` is a proposed storage kind, not an implemented declaration.
Its kind/codec belongs with shared Bloomery kinds, usable by native and WASM.
Use the existing kind derive and storage conventions.

Store the pinned initial kernel with `Batch::stage_bytes`, then establish
ordinary kernel and set head bindings. The stored artifact digest includes
the existing kind framing; it is not necessarily the raw-WASM hash.
`Journal::get_bytes` supplies the exact payload for the loader.

For an event, resolve both set membership and member heads at its journal
boundary. A set reference alone does not pin the members' moving heads.

Proposed exact boundary:

```text
event n:   select recipients from prefix n-1
           selected bundles fold their own views through n
           evaluate event n

head move at n:
           predecessor handles n
           successor warms through n and handles later events
```

A fold-only `EventBatch` rebuilds views; it does not issue historical effects.
Executing a historical event uses its historical selection, not the newest
head. Restoring a current view and re-executing historical events are distinct.

### Engine boundary and pending view input

Core engine scheduling, dispatch, registry publication, and actor lifecycle
semantics are not part of this implementation authorization. Changes to those
mechanics require explicit owner consultation and permission. In particular,
do not hold activation between `wire` and `Live` for a journal decision.

Actors activate through the ordinary engine lifecycle. The views actor owns
aggregation and pending journal input; reactor arms receive prepared inputs
only after the views actor can process the corresponding event. A live actor
need not have finished processing all its application input. No additional
engine readiness gate is needed to prevent unprepared reactions.

Use an ordered ring buffer for pending events, with allocated overflow for
bursts. Preserve FIFO across the ring and overflow, account for queued payload
bytes as well as event count, and release overflow storage after it drains.
An event awaiting a prerequisite remains pending; later events must not pass
it or affect its prepared view. Mail that supplies the prerequisite must still
be handled normally so the actor can resume. Fold-only history may establish
the missing prefix but must not skip pending live reactions. No silent event
dropping or new engine backpressure policy is authorized by this queue.

This corrects the withdrawn held-activation and retained-admission work orders
#6149, #6150, #6151, and #6153 (PRs #6152 and #6154). Those proposals are not
prerequisites. Buffering does not itself resolve historical selection after a
failed bundle replacement; the integration proof in section 6 remains required
before implementation of that handoff.

## 3. Shared lifecycle restrictions

Proposed API shape; final names follow existing code conventions:

```rust
ComponentBootstrap {
    prohibit: ComponentRestrictions::DROP,
    // Existing module, configuration, and routing inputs...
}
```

Flags are `DROP` and `REPLACE`; empty means both operations are permitted.
Combining them uses bitwise OR. This policy belongs to the stable native slot,
not replaceable guest memory or a mutable WASM manifest.

Check restrictions at the actual drop/replace handler before parsing a candidate,
running hooks, or changing state. A request forwarded by the component host
and one addressed directly to the trampoline must receive the same decision.
Replacement cannot clear restrictions. Creating another sibling does not
implicitly inherit kernel restrictions; it is a separate slot with its own
bootstrap policy.

Keep the first API on the trusted native bootstrap path unless a concrete
consumer needs a public load-mail field. Avoid an unnecessary wire-format
change. Unknown bits, if decoded anywhere, must be rejected rather than
silently converted into permission.

Prohibition of individual drop does not prohibit substrate/application
shutdown. Do not turn shutdown into a public per-component bypass.

## 4. Transactional replacement

The stable trampoline inbox already serializes replacement with guest delivery.
Reuse that boundary; do not add another scheduler or globally drain every
unrelated actor.

Required observable outcomes:

```rust
match replace(candidate) {
    Ok(success) => { /* successor serves the stable mailbox */ }
    Err(reason) => { /* predecessor still serves it; report rejection */ }
}
```

The implementation must distinguish preparation from publication:

1. Validate candidate bytes and export selection without changing the live slot.
2. Reach the existing quiescent handler boundary and capture migration state
   without irreversible changes to the predecessor.
3. Initialize and rehydrate the candidate, staging externally visible effects.
4. On rejection, discard candidate state and staged effects. Resume the
   predecessor with its prior usable state and routes.
5. On success, commit component state, metadata, aliases, and lifecycle effects
   coherently before admitting subsequent delivery to the successor.

These steps describe obligations, not an assertion that existing hooks already
satisfy them. In particular, `unwire` can emit mail and mutate guest state.
Moving `drop(old)` later is insufficient. The implementation plan must account
for guest-memory mutation, state capture, host calls, and buffered outputs.
If the current migration hook cannot provide a reversible snapshot, expose
that limitation and revise its contract explicitly; do not silently weaken
rollback or impose purity on arbitrary existing guests.

Do not call successor `wire` unconditionally: the established rehydration
path already restores children and other actor state. Duplicate wiring can
create duplicate effects.

Keep failures observable through typed results. The Bloomery adapter records
the corresponding event; the low-level component crate must not acquire a
journal dependency.

## 5. What source tracing established

| Surface | Finding |
| --- | --- |
| `trampoline/runtime/replace.rs::handle_replace` | Old instance is dropped before candidate instantiation; rehydrate errors still install the candidate. |
| `component/runtime/load.rs::begin_replace / finish_replace` | Host forwards a tracked request and relays the result; it does not restore a destroyed guest. |
| `Component::unwire / on_dehydrate` | Guest traps are logged; hooks do not currently provide a rollback veto. |
| Native dispatcher | Exclusive handler execution and a stable inbox provide the local ordering boundary. |
| SDK inline composition | Captures child identity/configuration/state and reconstructs children at their previous aliases. |
| Reactor generator | Generated peers are omitted from final exported types, while restoration searches that type list. |

Source links:
[replacement](../../crates/aether-component/src/trampoline/runtime/replace.rs),
[host forwarding](../../crates/aether-component/src/component/runtime/load.rs),
[hook behavior](../../crates/aether-substrate/src/actor/wasm/component/lifecycle.rs),
[dispatcher](../../crates/aether-substrate/src/actor/native/slot/dispatcher.rs),
[inline composition](../../crates/aether-actor/src/wasm/inline/compose.rs),
[generator](../../crates/aether-bloomery-reactor-derive/src/bundle.rs),
[restoration dispatch](../../crates/aether-actor/src/wasm/mod.rs).

Commit `57cdeb69a` removed the earlier predecessor-retention failure paths.
ADR-0016 already required rollback but acknowledged lifecycle effects before
rollback. Existing inline-child tests cover ordinary replacement; a focused
generated-reactor replacement reproduction remains to be run.

These are source findings, not newly executed runtime test results.

## 6. Kernel integration

Bootstrap loads pinned genesis bytes through ordinary component machinery
with `prohibit: DROP`. Kernel rules produce typed lifecycle intents. Native
adapters load exact journal artifacts, execute the operation, and return
outcomes as events.

Bind each delivery to journal position, cluster head, selected artifact, and
the current activation attempt. A delayed reply must not mark a later
A-to-B-to-A activation ready. Distinct cluster heads pointing at identical bytes
still have distinct view state.

Use existing fold-only batches to prepare views before live delivery. Preserve
event order; concurrent program execution is not a reason to wait for every
program to finish before handling another journal event.

A rejected replacement leaves the application resident and usable. It must
not falsely acknowledge that the requested bytes became active. In particular,
if a journal head already selects a failed candidate, the adapter cannot
silently evaluate an event selected for that candidate using the predecessor.
The integration work order must demonstrate how rejection, subsequent
selection, and any corrective head event fit together. Historical head
resolution remains authoritative; outcomes do not create a second activation
history.

This is a required integration proof, not permission to invent governance or
claim that prohibition flags validate all journal writes. Likewise, keeping
the kernel in the effective reactor set is distinct from preventing its
runtime component from being dropped.

## 7. Scope and verification

Required scenarios include successful replacement, each preparation failure,
a predecessor still answering afterward, no leaked candidate mail/aliases,
peer identity restoration, prohibited direct and forwarded operations, normal
shutdown, historical head selection, and stale activation replies.

Excluded: general head governance (#6133), view-memory optimization (#6132),
program scheduling, exactly-once external effects, and full crash recovery.
A view cursor is not a durable execution checkpoint.

Local work is authorized while Eve is offline. Prefer GitHub CI for expensive
WASM/build/runtime checks. These documents do not claim those checks have run.
