# ADR-0223: Journal-Selected Kernel Reactor

- **Status:** Proposed — owner approved for implementation; PR review pending
- **Date:** 2026-09-17

## Context

Bloomery needs versioned logic to manage the reactor clusters that handle
journal events. That logic must use the same WASM reactors, bundled views,
head references, and actor lifecycle as application reactors.

[ADR-0220](0220-the-journal-is-an-append-only-log-of-typed-events.md) supplies
typed heads and immutable artifacts. [ADR-0222](0222-reactor-bundles-own-their-views.md)
places views and reaction logic together in each WASM bundle. A stable mailbox
does not identify the version of code behind it.

The current component replacement implementation can return an error after
dropping the predecessor. This regresses the predecessor-retention behavior
removed in commit `57cdeb69a` and contradicts the rollback requirement of
[ADR-0016](0016-persistent-state-across-hot-reload.md). That older implementation
also acknowledged teardown effects before rollback; merely retaining the old
object does not establish transactional behavior.

## Decision

Events record things that happened. A reactor head move records successful
activation or replacement; it does not request that operation. Attempt the
operation first. On failure, record rejection and preserve the existing head.
Never publish the candidate head and then try to make that fact true.

The kernel is a required, replaceable WASM reactor bundle. Genesis is a pinned
build of that kernel, stored and copied as ordinary immutable bytes. It does
not introduce a special executable format or a second rule engine.

An active reactor set contains cluster heads. Each head identifies its bundle;
all generated peers in a cluster share that bundle selection. For historical
event execution, resolve the set and member heads at that event's boundary,
not against today's heads. The companion proposes the precise before/after
boundary.

Kernel rules observe recorded events and emit lifecycle intents. Existing
native component machinery performs the operations. Lifecycle outcomes become
observable events, including rejection, so application reactors, agents, and
humans can respond.

Observing the resulting head move must not request the same replacement again.
The delivery boundary for that observation requires reconciliation with the
already completed replacement; the earlier predecessor-recipient proposal is
withdrawn pending that proof. This does not authorize engine lifecycle changes.

Establish the shared component replacement invariant:

> Replacement either commits a functioning successor or rejects while
> preserving the predecessor's usable state and bindings. Failed preparation
> must not publish partial lifecycle effects.

Use the existing serialized mailbox and peer restoration machinery. Quiesce
at the existing handler boundary before attempting replacement. Teardown,
candidate initialization, and rehydration must respect the transaction;
logging an error after destroying the predecessor does not satisfy it.
Do not claim arbitrary WASM or host failures are physically impossible.
The host must not commit a failed kernel transition.

Add component lifecycle prohibition flags at bootstrap. Illustrative API:

```rust
prohibit: LifecycleFlags::DROP
// Other components can prohibit both:
prohibit: LifecycleFlags::REPLACE | LifecycleFlags::DROP
```

The stable host slot owns the restrictions and preserves them across replacement.
Check them before hooks or mutation, including directly addressed operations.
Guests cannot relax them. Ordinary components default to no prohibitions.
Kernel bootstrap prohibits individual drop; orderly whole-application shutdown
still works. This is lifecycle policy, not a general security or head-governance
system.

Repair shared replacement rather than fork the trampoline for kernel-specific
rollback. General head governance remains parked in
[#6133](https://github.com/iamacoffeepot/aether/issues/6133).

## Consequences

- The initial kernel is reproducible and subsequent selection is journal-defined.
- The host and WASM/mail ABI remain compatibility obligations.
- Historical event execution requires historical artifacts and head bindings.
- Transactional rejection becomes a shared component guarantee.
- Candidate and predecessor may coexist during preparation, increasing peak memory.
- Prohibition flags prevent an individual lifecycle operation; they do not alone
  validate reactor-set membership or authorize journal writes.
- Generated peer reconstruction must be repaired before kernel integration.
- Durable program execution and exactly-once external effects remain outside scope.

The [design](../design/bloomery-kernel-reactor.md) separates agreed invariants
from implementation proposals. The [work orders](../design/bloomery-kernel-work-orders.md)
are review drafts, not dispatched tasks.

## Alternatives considered

- **Native unversioned kernel policy:** loses journal-selected behavior.
- **Custom kernel trampoline fork:** duplicates replacement machinery when the
  identified failure applies to all components.
- **Panic in guest teardown to veto removal:** traps are contained and teardown
  continues; admission must happen before hooks.
- **Separate numeric policy settings per operation:** use composable prohibition
  flags as requested.
- **General governor framework:** independent exploration, not a prerequisite.
