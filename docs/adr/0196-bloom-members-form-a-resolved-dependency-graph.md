# ADR-0196: Bloom Members Form a Resolved Dependency Graph

- **Status:** Proposed
- **Date:** 2026-08-15

## Context

Two serializations throttle bloom throughput, and both come from the same
missing primitive: members cannot state that one depends on another.

First, dependent work waits whole wave cycles. A workpiece that builds on a
sibling's output (a retry-seeding feature on top of the checkpoint machinery,
a calibration read on top of a price-table reshape) cannot co-seal with it,
so it queues for the next bloom — a full seal → construct → verify → review →
weave → land cycle of dead time per dependency hop.

Second, known collisions are handled as if they were surprises. Two members
whose declared surfaces overlap are either kept out of the same bloom by the
operator (more waves) or co-sealed and left to collide at fold time, where the
conflict dispatches a Reconcile lane (ADR-0189) and spends repair budget on an
ordering that was knowable at the seal door. One live bloom burned a member's
entire Reconcile budget on a single logical collision.

ADR-0185 attacked the first problem across blooms: a train, where a successor
bloom seals against the projected post-land tree of its predecessor. It was
never implemented. Its central observation survives and is load-bearing here:
**proof identity is tree-shaped** — the post-land commit is unknowable before
the squash, but the tree is exactly the resolved proposal's tree, and the
correspondence store already resolves commits to trees everywhere the source
port touches git. The composite gate already passes aggregate verify by tree
identity when the folded tree was proven (#4891).

Meanwhile ADR-0191 settled what a bloom's integration is: the composition of
workpieces is itself a workpiece — the weave is a construct, composite gates
are its verify, and members are immutable once their review passes. A member's
resolved candidate is therefore a stable, tree-addressed artifact the moment
it resolves. That is the property a dependency graph needs.

Cost pressure shapes the same design. Measured on live waves: a dependent
construct that starts cold re-derives its dependency's context from the diff
(the forensics tax that per-member descriptions removed for member identity
reappears as dependency archaeology); cold lane builds run ~6 minutes against
~1 minute warm in a slot whose target directory already built the base tree
(#4917); and session-resume economics measured on the session pool (#4902)
showed that resume priced purely as a cache play loses on Claude seats — the
recurring cache-read toll on carried context outweighs the one-time
prefix-write saving — while the real payoff is fewer turns, which requires
the carried context to be *the right context*. A dependency edge is precisely
the case where it is: the session that built A already holds the tree B
builds on.

## Decision

A bloom's members form a directed acyclic graph, resolved at the seal door,
scheduled by readiness, based by splicing, and landed exactly as today.

### The graph

A member may declare dependencies on sibling members of the same bloom. The
seal door resolves the full edge set as the union of:

- **Declared edges** — stated in the seal request, member → member.
- **Derived edges** — for every pair of members whose declared surfaces
  intersect, an ordering edge in seal-listed order. A collision that is
  visible at the door becomes deterministic sequencing instead of a fold-time
  Reconcile lottery.

The door refuses a cyclic graph with the cycle named, refuses an edge naming
a workpiece outside the bloom, and journals the resolved graph as part of its
decision (ADR-0190: the record is what was decided; replay folds it). A bloom
with no edges is the degenerate case and behaves exactly as blooms do today —
the graph is a strict generalization, not a new mode.

On the wire the edge set is additive: a new tail-appended value in the seal's
effect vocabulary carrying `(member, depends_on)` pairs. `Membership` itself
does not reshape — its fields are wire-frozen in the Decisions graph
(ADR-0187/0190; the #4942 incident is the cautionary case), and the golden
decisions fixture must show a pure append.

### Scheduling

A member's construct dispatches when every dependency is **resolved** — its
candidate verified and its review passed, the ADR-0191 immutability point —
and a lane slot is free. Root members dispatch at seal, as today. Readiness
is folded from journaled resolution facts, never re-decided, so a restarted
coordinator recovers the schedule from replay.

A wedged member blocks only its descendants. Independent subgraphs run to
resolution regardless. A supersession can drop a wedged subtree (the
member-drop supersede that already exists) and the successor's weave covers
the remainder; claims for untouched members transfer at their scope revisions
exactly as today.

### Splicing

A dependent member's construct base is the bloom base with each dependency's
resolved candidate spliced in, in topological order — the same splice the
ADR-0191 weave performs at integration, applied earlier and per-member. The
consequences:

- **Determinism.** Splice order is the resolved graph's order, journaled at
  seal. Two replays assemble byte-identical base trees.
- **Proof identity.** A dependent member's verify proof binds to its spliced
  base tree. Because candidates are immutable after review, that tree cannot
  drift under the proof. The final weave's fold must equal the composite of
  all candidate splices, and the existing tree-identity gate (#4891) checks
  it without re-running verification the members already carry.
- **Residual conflicts.** Splicing two dependencies that collide with each
  other is the fold-conflict class; derived edges make most such pairs
  ordered before it can happen, and what remains dispatches Reconcile
  (ADR-0189) on the dependent member's base assembly — journaled, budgeted,
  unchanged in semantics. The graph shrinks Reconcile's caseload; it does
  not retire the stage.

### Session and slot affinity along edges

An edge is the unit of reuse. When member B depends on member A:

- **Slot affinity.** B's construct prefers the lane slot that built A: the
  per-slot target directory (#4917) has already compiled the base tree, so
  B's builds run warm.
- **Session affinity.** B's construct may resume the session of A's
  construct, under the standing rules the session pool already enforces:
  same harness and model seat only; the pool keys sessions by slot path and
  resumes only in the same slot (a resumed harness misbehaves if its cwd
  changed); the resumed prompt states that the tree was reset and what was
  spliced; two failed resumes fall back to fresh. **A judge never resumes a
  builder's session** — review independence outranks any saving, so Review
  and AggregateReview seats always start fresh.
- **Honest economics.** Every dispatch journals `fresh` or `resumed` with
  the per-call token evidence, priced from the sealed PriceTable — never the
  harness's self-reported figure. The acquire decision is computed from
  sealed rates and learned parameters (predicted-versus-actual is logged and
  read back), so the policy earns its defaults from the calibration ledger
  rather than assuming them. The break-even the pool measured stands: on
  seats with free cache writes, fresh is often cheaper than resume — the
  edge case pays through reduced turns, and the evidence must show it or the
  policy backs off.

### Preventing wasted work

The graph turns waste from an accident to be repaired into a state that is
either unreachable or checkable. Four commitments:

- **No speculative spend.** Nothing dispatches before its prerequisites
  hold. A member whose dependency is still constructing costs nothing; a
  member whose dependency wedges never starts. This is the structural
  difference from the train, which paid for projection misses — readiness
  gating spends money only on work whose ground truth already exists.
- **Resolved work is never redone.** Candidates and their proofs are
  addressed by tree. A verify whose target tree already carries a proof
  passes by identity — the #4891 gate generalized from the aggregate stage
  to every stage. Supersession already adopts resolved candidates at zero
  re-construction spend; with the graph the transferable unit becomes the
  resolved subtree, so a successor bloom re-runs only the wedged branch and
  inherits everything upstream and parallel to it, proofs included.
- **Partial work survives failure.** A wedge strands its subtree, and only
  its subtree — every resolved member still lands through the weave. Below
  the member, lane-death recovery composes with the graph: a retry seeds
  its worktree from the member's newest checkpoint (#4934, #4994) and
  re-splices the same dependency candidates, so a died lane forfeits
  minutes, not the member.
- **The repair class shrinks at the door.** Derived edges convert the
  collide-then-Reconcile spend — construct twice, conflict, repair — into
  an ordering that costs nothing. What Reconcile still handles is the
  genuinely unforeseeable residue.

### What does not change

The landing unit. One weave, one landing proposal, one squash onto the day
branch, one land receipt — composition is a workpiece (ADR-0191) and this
decision does not split it. Members' claims, approvals, scope revisions, and
supersession semantics are untouched. The graph changes when constructs run
and what trees they stand on; it does not change what lands or how.

## Consequences

- The whole board can seal as one bloom: independent members run as wide as
  the lane ceiling allows, dependent members chain without inter-wave dead
  time, and known collisions cost an ordering edge instead of a Reconcile
  lap or a wave of exclusion.
- ADR-0185's train becomes unnecessary in direction: the cross-bloom
  projection it built is subsumed by in-bloom edges. ADR-0185 remains
  unimplemented and should be marked superseded by this record when this
  one is accepted; its tree-identity observation is carried forward here.
- The seal door grows a resolver (cycle refusal, edge derivation) and the
  scheduler a readiness fold — both O(members + edges) over journaled facts
  at board scale (tens of members, sparse edges), with no new persistence
  beyond the appended edge value.
- Wedge blast radius shrinks from the whole bloom to a subtree, which makes
  large blooms operationally safe — the precondition for board-sized seals.
- Session and build-cache reuse get a principled trigger (the edge) instead
  of a global default, with journaled evidence either paying for the policy
  or retiring it.
- Follow-on work: the edge vocabulary and seal-door resolver; the readiness
  scheduler; splice-based lane provisioning; edge-affinity acquisition in
  the session pool; `xtask bloom seal` authoring for edges; a validation
  wave replaying landed history through the graph path before edges become
  the default authoring shape.

## Alternatives considered

- **The bloom train (ADR-0185)** — pipelines blooms against projected trees.
  Rejected as the primary mechanism: it keeps the bloom serial inside and
  multiplies operator surface (supersede-on-projection-miss) instead of
  removing the barrier; its useful core (tree-shaped proof identity) is
  absorbed here.
- **Reshaping `Membership` to carry edges** — rejected: those bytes are
  wire-frozen in the Decisions graph; the edge set rides as a tail-appended
  value instead.
- **Incremental landing of completed subgraphs** — landing each resolved
  antichain as its own squash. Rejected for now: it fractures the landing
  unit ADR-0191 just unified, multiplies sync-back traffic, and the revert
  granularity argument that motivated it is served well enough by
  member-drop supersession.
- **Global session reuse (resume everything by default)** — rejected by the
  measured economics: without an edge the carried context is the wrong
  context, and on free-cache-write seats fresh is cheaper. Reuse follows
  the graph or does not happen.
