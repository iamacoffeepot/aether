# ADR-0217: Coalesced aggregate pre-checks

- **Status:** Proposed
- **Date:** 2026-09-09

## Context

Captured candidate trees exist before every member has completed its own
verification. Today the aggregate gate waits for all claims and the ordinary
fold. That postpones interaction feedback and leaves idle prover capacity
unused near the end of construction.

The measured warm-slot pair took 52.963 seconds serially and 32.310 seconds
composed (1.639x). Both members changed the same package with warm
dependencies. The saving was one changed-package/test-binary rebuild; it is
not a fleet throughput estimate or a universal accumulation deadline.

This is slice 2a of the batch-verification design. Physical slot sharing with
standalone member proofs remains independently useful. Contextual member
proofs, survivor groups, inherited construction heads and eager partial
integration require their own changes. This slice preserves the member
verification and final membership rules in ADR-0189 (`Fact::FoldConflict`,
`StageId::Reconcile`) and ADR-0196 (`MemberDependency`).

ADR-0200's discriminated proof ledger remains the local verifier's authority
for reusable facts. The reducer's existing whole-verification memo is a
different layer: `VerifyProof`, `VerifiedTree` and `BloomRecord::verify_proof_for`
record a passed stage over an exact tree and sealed gate set. This decision
uses both existing paths and creates no second proof-receipt database.

## Decision

### Sealed, optional scheduling

`PrecheckPolicy { run_budget }` is a bloom-wide configuration. Absence disables
the feature; zero is invalid. The policy is immutable after seal and bounds
speculative physical attempts. It has no learned clock estimate and no
two-member maximum. A plan needs at least two captured, eligible candidates.

The reducer builds `PrecheckPlan` in sealed member order from candidates
currently in Verify and current claimed candidate vehicles. It pins the
bloom, sealed base, each workpiece's scope revision and `CandidateRef` (tree
and checkout), and the aggregate gate-set digest. A changed version changes
the plan identity. Constructing or repairing versions are excluded.

`QueuePrecheckPlan` is coalesced before preparation. Only the newest pending
plan is prepared; obsolete unstarted plans are acknowledged without doing Git
work. Preparation runs on a bounded background worker. It folds exact pinned
checkouts in a namespace derived from the plan digest. `HostSource::integrate_pinned`
checks that each checkout contains its named tree and creates private immutable
candidate pins. It never resolves the member's moving candidate ref. Ordinary
integration refs and membership claims are untouched.

A prepared node records the plan, tree, checkout head and gate set. A merge
conflict produces `PrecheckPreparation::Refused` with retained diagnostics.
It does not revoke or fail any member's standalone claim.

### Idle-only physical execution

The reducer offers the newest prepared node. The executor requests it only
after required work has been considered and a backend reports idle capacity.
Admission is checked again when the local slot is reserved; speculative work
never enters the ordinary waiting queue. Backends without an idle-capacity
contract decline speculation.

`RequestPrecheck` records the issued immutable node and spends one policy
attempt before `DispatchPrecheck` can run. At most one issued speculative
run exists per bloom. A newer plan can replace the prepared node while an
older run finishes, but cannot displace the immutable issued input.

A budget of one favors the earliest idle interaction check. If it runs a
partial plan before later members finish, that attempt leaves no budget for
the larger plan. Final-plan joining then requires a larger sealed budget or
the first idle opportunity to arrive after the full plan is available. This
policy bounds speculation; it does not reserve an attempt for final coverage.

The order uses `StageId::AggregateVerify` and the normal aggregate command.
Trusted host metadata distinguishes it from the final aggregate order:
the workpiece is `WorkpieceId::composition()` and `scope_revision` is the
node digest. Its displayed subject is the node tree. The sealed configuration
registry is carried through the outbox into the outstanding order so the
local lane selects the gate program the node names.

Offer requests and skipped completions are retained on the existing outbox
row before admission. The row is acknowledged only after its event key is
journaled. Offer and preparation keys include the outbox sequence, allowing
an explicitly re-offered node to retry after a host fault or operator hold and an earlier
candidate set to become current again after a sibling enters repair.

An idle submission first records `Submitting`. If it becomes obsolete or is
promoted, `settle_idle_submission` must settle the original offloaded call
before deciding what to do. Once promoted, a required submission must return
its own answer before the order is acknowledged; an inspection cannot stand
in for unfinished preparation. Promotion preserves the original deadline.
A call that has not started becomes a probe;
a running call returns its actual handle. A restart probe never starts an
absent obsolete order. Only a confirmed unstarted order can emit
`SkippedBeforeStart` and refund its speculative attempt. Started stale runs
remain tracked until their physical result is accounted for. Deadline cancels
discard unstarted idle calls and wait for started submissions before cancelling
their process. Uploaded and timeout completions are retained on the same
dispatch outbox row before the order is consumed, so a lost admission cannot
recreate an expired order.

### Proof and failure ownership

`PrecheckCompleted` binds to the issued node, its exact subject and its sealed
aggregate contract. Passed/failed evidence must be a verification result;
a host fault must be executor-fault evidence. Worker uploads cannot choose
the node identity or change attribution.

A passed node can file an aggregate `VerifyProof` through the existing memo.
It never files a member Verify proof. A stale passed node can still supply
the proof it actually earned, but cannot make the newest node green or change
any current candidate. A speculative red belongs to the composition alone.
Its findings are retained under the node's key in the existing findings cache;
they do not become a member's repair order.

When the ordinary final fold produces the same tree under the same contract,
an existing aggregate proof satisfies its mechanical gate. An exact issued
candidate plan can instead be joined, with the critic dispatched concurrently.
The joined run becomes required work, so a busy prover cannot leave the final
fold waiting indefinitely for speculative admission.

Checkout head equality is not proof identity: preview and ordinary folds
have separate namespace-specific head identities. Both checkout vehicles must
contain the same verified tree, and the full member plan, base and sealed
gate contract must match. The ordinary fold's head remains the landing head.

A joined red enters the existing composition repair path. Only then are that
node's retained findings restored to the composition work order. Restoration
is idempotent and preserves other current findings. A host fault remains a
machinery failure and cannot spend a code-repair lap.

### Replay, cost and operator view

`RecordPrecheckState` is the replay authority. Host projections read recorded
decisions incrementally, in bounded pages, and do no speculative work from an
incomplete replay. New facts and decisions append to the wire vocabulary.
Prior journal schema digests have explicit upcasts, and queued view rows retain a previous-shape
decoder for their positional bloom elements. Absent-policy histories retain
their existing behavior.

Physical dispatch and cost belong once to the bloom. Joining a running proof
does not dispatch or charge another physical run. Members retain their own
standalone verification and latency accounting. A skipped admission has no
physical execution cost. Speculative work and final memo reuse remain visible
as distinct observations so moving work earlier is not reported as eliminated
compute.

`BloomView.precheck` projects the journaled state. The console board shows
preparing, pending, running, green, red, stale, joined or paused, with the
joined head, or the prepared head when there is no final join. Full plan,
node, budget, result and diagnostic identities remain available in `/view`
and `/journal`.

The existing terminal janitor derives private namespaces from retained
preparation outbox rows. It waits for preparation acknowledgement and all
outstanding orders of the owning bloom before pruning those refs, and uses
the existing per-tick prune bound. Evidence and proof records are retained.

Bloom membership stays atomic at Resolve. This change does not land members
as they finish or authorize constructing on unverified inherited heads.
Independent landing would change membership and supersession policy.

## Consequences

Interaction errors can surface before the final fold, and a matching run can
leave only its remaining latency on the final mechanical path. Required work
has priority, obsolete pending plans do not consume full-suite runs, and
speculation is bounded by the sealed policy.

A pre-check that becomes obsolete still costs the work already performed.
Moving one aggregate run earlier and later reusing it usually changes latency,
not the number of physical suite runs. Measure final-tail reduction, earlier
interaction feedback, avoided duplicate dispatches and wasted speculation
separately before enabling a larger policy budget.

The fleet remains unchanged until an updated coordinator is deliberately
deployed and a future bloom seals the policy. This implementation does not
restart a live bloom or rewrite its configuration.

## Alternatives considered

- Verify every captured head: pays one aggregate run per arrival rather than
  coalescing pending work at idle capacity.
- Queue speculative orders normally: can put required member work behind an
  obsolete aggregate run before the scheduler can reconsider it.
- Use moving member refs in preview plans: a repair can silently change the
  tree the supposedly immutable plan executes.
- Mint standalone member proofs from a composed pass: changes proof semantics
  and attribution, reserved for the contextual-proof slice.
- Add a second receipt store: duplicates ADR-0200 and the existing outbox's
  crash/admission protocol without adding authority.
- Hard-code a 20.6-second wait: mistakes one same-package rebuild saving for
  a measured fleet scheduling threshold.
