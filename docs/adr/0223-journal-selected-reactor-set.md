# ADR-0223: Journal-Selected Reactor Set

- **Status:** Proposed
- **Date:** 2026-09-17

## Context

[ADR-0220](0220-the-journal-is-an-append-only-log-of-typed-events.md)
establishes an append-only journal, typed heads, and immutable artifacts.
[ADR-0222](0222-reactor-bundles-own-their-views.md) puts each reactor's code
and view definitions in its bundle. Bloomery needs to select those bundles
at historical event boundaries and change a running selection without losing
the predecessor's work.

The earlier proposal made a special kernel bundle mandatory and replaced
components in place. That coupled journal selection to shared engine
replacement rollback, lifecycle prohibition flags, and generated peer
restoration. None is needed to route to a second, independently loaded
instance. This revision supersedes that proposal; it does not change the
separate engine obligations in [ADR-0016](0016-persistent-state-across-hot-reload.md).

## Decision

The journal is the durable state. Head bindings, selected reactor sets,
activation receipts, and reaction progress are folds over recorded entries.
Runtime actors may cache those folds, but must reconstruct them after restart.

A `ReactorSet` contains cluster heads and may be empty. No member is reserved
or required. For an event at sequence `N`, select the set and each member's
head from the prefix through `N-1`. A head move recorded at `N` is itself
delivered to the predecessor. The successor owns events from `N+1` onward.
This boundary is the selection rule, independent of when the successor
finishes loading.

Each selected version runs in an independent instance identified by its
cluster head and artifact digest. Load the successor beside the predecessor,
fold eligible history through `N` without reactions, then route live input
to it. Never replace the predecessor in place. An instance no current head
names is dormant; after a later move has an activated receipt, it may be
dropped. Keeping a previous instance warm is only a cache policy.

The native feeder converges loaded instances and routes toward journal head
bindings. It uses ordinary `LoadComponent` and `DropComponent` operations.
Loading, warmup, routing, and retirement are lifecycle work, not a second
source of policy. A rejected activation is recorded and leaves the head move
in the journal. It neither rewrites that move nor silently skips the interval
the failed version would have owned. The next successfully activated owner
must receive that interval as live work. A new move back to an older head is
an explicit journal decision.

Policy such as admission, membership, or moving back after rejection belongs
in rules of ordinary bundles selected in the set. A fresh journal can begin
with an empty set. There is no special kernel executable, `KERNEL_HEAD`,
required member, or `KernelMissing` selection error.

Lifecycle requests and outcomes are journal facts with cause and identity.
The proposed `core.cluster.activate` and `core.cluster.retire` programs use
the durable `Requested` → execution → `Transition` path; a `Fault` records
an attempt that did not finish. Activation yields `Activated` or
`Rejected { reason }`. Each recorded reaction must identify its trigger,
reactor instance, and rule so a restart can derive outstanding work and
deduplicate by fold. The exact encoding, source validation, and recovery
proof belong to the companion design and bounded implementation plans.

## Consequences

- A historical event uses the set and artifact bindings selected at that
  event's boundary. Rebuilt source code cannot change an old bundle's views.
- Candidate loading cannot destroy the predecessor, but coexistence consumes
  memory until dormant instances are retired.
- An empty set is a valid operating state. Native feeder convergence runs
  even when no policy bundle is selected.
- Component replacement defects remain real for ordinary hot reload and
  should be addressed on their own merits, outside this reactor-set arc.
- Exact-once live evaluation, attribution, instance reuse, and crash recovery
  are requirements to demonstrate. This ADR does not assert they already
  hold in the current implementation.
- This decision authorizes no engine or chassis change. Application
  composition and bootstrap wiring need a separate scoped decision.

The [design](../design/bloomery-reactor-set.md) records the handoff and open
proofs. The [work orders](../design/bloomery-reactor-set-work-orders.md) are
proposed slices, not implementation authorization.

## Alternatives considered

- **Transactional replacement in one stable slot:** requires shared engine
  rollback and lifecycle restrictions for an isolation property that
  side-by-side routing supplies directly.
- **Move the head only after observation or installation:** leaves an
  ownership interval that the journal prefix cannot describe. Record the
  move first and make installation outcome observable.
- **Required policy bundle:** makes an empty set invalid and duplicates
  native convergence. Ordinary bundles can express optional policy.
