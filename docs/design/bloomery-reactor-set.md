# Journal-selected reactor set: design for review

- **Status:** Proposed; implementation requires separately scoped issues
- **Date:** 2026-09-17
- **Decision:** [ADR-0223](../adr/0223-journal-selected-reactor-set.md)
- **Plan:** [Work orders](bloomery-reactor-set-work-orders.md)

## Selection and handoff

The journal selects a set of cluster heads. At event `N`, selection reads the
prefix through `N-1`; the predecessor observes a move at `N`, and the
successor owns `N+1` onward. The landed selection work in PR #6141 is the
starting point. The set may be empty, and selection has no required member.
Historical execution resolves both set membership and member head bindings
at the event boundary; today's bindings cannot stand in for either.

The feeder is a native application actor. It pages the journal in contiguous
order, reconstructs selection and activation receipts, and routes to a
separate instance for each `(cluster head, artifact digest)`. On a cache miss
it reads the exact artifact, loads the bundle's cluster export, warms eligible
history with fold-only `EventBatch`, and starts live `Event` delivery after
the boundary. `EvaluatedResult` acknowledges live peer evaluation; mere
preparation or lifecycle settlement is not that acknowledgment. Delivery
for a cluster cannot advance past `N` until evaluation of `N` is known.

A move at `N` requests activation. The predecessor must evaluate `N` before
the successor receives live `N+1`. If loading or warmup rejects, the move
still stands in the journal. The predecessor remains usable for its own
selected interval, and no reaction in the failed version's interval is
discarded. A later successfully activated owner receives the owed interval
as live work, in order. Multiple moves are realized in sequence, rather
than jumping to the latest target. Retirement drops only an instance no
current head names, after a later activated receipt makes that safe.

The feeder's load and drop operations use existing component mail. Its
convergence is native plumbing, not a policy rule. Membership changes,
admission, and a move back after rejection can be expressed later by rules
in ordinary bundles. No bundle is mandatory at genesis or thereafter.

## Durable lifecycle and attribution

Proposed declarations are `core.cluster.activate { cluster, artifact }` and
`core.cluster.retire { cluster }`, executed first by the native feeder. An
activation result is `Activated | Rejected { reason }`; neither result is a
head move. A request records the trigger cause, reactor instance, rule, and
input reference. The driver chooses the executor, records a `Transition`
for a completed result or `Fault` for an unfinished attempt, and can derive
open requests from the journal after restart. A durable reaction identity
must let the writer reject a duplicate `(cause, reactor, rule)` by fold.
These are proposed kinds and responsibilities, not current API claims.

Executor selection should mirror reactor selection through an `ExecutorSet`
with no required member. The declaration identifies a program; an artifact
is one executable implementation. Native executors can precede guest
executors. A future `Pure` guest has read/stage access only; a future
`Sampled` guest is an ordinary component. Those guest paths and any linker
work require their own decisions and plans.

## Proofs required before implementation claims

1. **Attribution and deduplication.** Show the emitted mail preserves trigger
   sequence, reactor instance, and rule through recording, and that a replay
   derives at most one durable request for that identity. Do not infer source
   identity from reply correlation alone.
2. **Crash recovery.** Inject crashes between request, execution, receipt,
   load, warmup, and route flip. Derive the same selected route and every open
   reaction from the journal. A `Fault` cannot be mistaken for a completed
   result; a result cannot be recorded twice for one request.
3. **Live ownership.** Demonstrate predecessor evaluation at the move,
   successor evaluation from `N+1`, and late live delivery of every failed
   activation interval. Fold-only warmup must emit no effects or count as
   completed live evaluation.
4. **Instance collision and reuse.** Prove that runtime names do not collide
   when digest prefixes match, and distinguish instances for identical bytes
   under different heads. For `A → B → A`, prove whether a dormant A can
   safely reuse its view cursor or must be reloaded and replayed. A stream
   token including move sequence must reject late replies from B or an older
   activation of A.
5. **Composition.** Resolve how the application starts the journal owner and
   feeder and establishes an empty or first-bundle set. Chassis boot wiring
   is outside this arc's allowed surface and requires a separate, explicitly
   scoped decision. This design does not authorize a boot edit.

The current `EventBatch`, `Event`, `EvaluatedResult`, and
`ClusterStatusQuery` interfaces provide useful seams, but none alone proves
these properties. Trace tests must judge the final journal and delivered
sequences, not just individual handler replies.

## Scope and related work

This reactor-set arc changes Bloomery application crates and docs only. It
does not change `aether-substrate`, `aether-actor`,
`aether-actor-derive`, `aether-component`, `aether-behavior`, or
`aether-chassis-*`. An engine defect discovered here needs its own failing
test, scope, and owner decision; it is not an implicit dependency of this
design. ADR-0016's ordinary replacement contract still applies to ordinary
replacement users.

As of `origin/main` `b4b7e41`, PR #6141 has landed. PRs #6140, #6142,
#6143, and #6147 were reverted by #6172. PRs #6170, #6148, #6165, and
#6163 are closed; #6146 is being closed. This history is disposition, not a
new implementation order. The companion [work orders](bloomery-reactor-set-work-orders.md)
list the remaining proposed slices.
