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

### Session, checkout, and slot

Three things were once keyed to the same axis. They have different subjects and
are now separated (amended 2026-08-22, #5425/#5427).

**The checkout belongs to the session.** Every harness binds a conversation
permanently to the directory it was born in — grok stores sessions under a
percent-encoded working directory and ignores `--cwd` on a resume, Claude Code
keys `~/.claude/projects/<encoded cwd>` the same way, codex likewise. The
executor therefore assumes it of every harness and gives a session its own
tree: `<scratch>/sessions/<slug>/tree`, created when the session is minted,
reused by every launch of that conversation, removed by the janitor once no live
member is bound to it.

The slug is a `SessionSlug` the *coordinator* mints at pool acquire, before the
cold launch, because the harness's own id does not exist until after it. One
format for every harness: it names the directory, keys the `construct_session`
row, and rides the dispatch evidence, while the harness's native id is a
recorded attribute of that row. It is also what the re-adopt record carries
beside the slot, so a coordinator that restarts mid-dispatch re-attaches a
surviving child to the tree it is actually working in.

A session outlives one workpiece. Along a declared edge the dependent's
construct resumes the predecessor's conversation *in the predecessor's tree*,
which the lane resets to the dependent's own base and splice in place; the
resumed prompt states the reset and what was spliced. That is why the tree
cannot be keyed to a workpiece either.

**One dependent per session, because a tree holds one lane** (amended
2026-08-24, #5425). The graph fans out, and a chain is only its narrow case: a
predecessor with edges to two members unblocks both on one admission and they
dispatch in the same tick. The inheritance is therefore exclusive — the first
dependent to prepare continues the predecessor's session in its tree, and a
sibling that finds that slug already bound to another member mints its own and
launches cold. The check and the record share one lock scope over the journal,
so two dispatches racing cannot both read the slug as free, and the answer is
durable rather than in-memory: a restarted coordinator reads the same owner
instead of re-deciding it. The resume follows the tree rather than the edge —
a harness edits the directory its conversation was born in whatever `--cwd`
says — so a sibling that did not inherit the tree does not resume the handle
bound to it either, and journals `session_taken` as its miss. What that sibling
pays is one cold launch. What it would otherwise lose is its work: two lanes in
one checkout reset and clean over each other, and the capture that follows can
commit the union of two members' edits as one candidate.

The harness-specific surface is only the per-harness argv builder, and it
exposes two shapes: a cold launch, which states the directory the session is
born in, and a resume, which states the harness session id and no directory.
Everything above it — slug minting, the tree's lifecycle, the between-workpiece
reset, the resumed-prompt preamble, the lent target, the post-run
dirty-sibling gate — is executor-level and identical for every harness.

**Slot affinity is the member's own.** A member's next lane prefers the lane
slot its last one ran in: that slot's target directory has already compiled
*this workpiece's* crates, and no other slot has. Keying it to the edge was the
wrong axis twice over — a member's own second lane is the commonest case and had
no preference at all, and the lookup key (the checkout hex the lane would build
on) changed at every capture, so even a dependent's lookup mostly missed. The
preference is never a wait: a busy or quarantined slot falls back to the lowest
free index. What the slot is, now that it owns neither the tree nor the session,
is a concurrency token and a warm cargo target directory lent per dispatch as
`CARGO_TARGET_DIR` — which the lane exports into the model's own environment,
because a child that builds anywhere else misses the compiler cache on the whole
dependency tree.

**Session affinity follows the thread, and only the graph widens it.** A session
belongs to one thread — one (workpiece x role). A member's own retry laps
continue their own session; a dependent's first construct may resume the
journaled session of a predecessor it declares an edge to, under the rules the
pool already enforces: same harness and model seat only, a resumed prompt that
states the tree was reset and what was spliced, and two failed resumes falling
back to fresh. **A judge never resumes a builder's session** — review
independence outranks any saving, so Review and AggregateReview seats always
start fresh.

What is retired is the slot fallback (#5427): the pool also kept the last
builder session deposited *at each slot path* and handed it to any cold
construct that landed in that slot with a matching seat, with no workpiece check
and no declared edge. Two unrelated members dispatched into one slot six minutes
apart therefore shared a conversation — the second opened carrying the first's
whole history and then deposited the first's session id as its own. A directory
was never a member identity. A first construct with no session of its own is
fresh unless the graph says otherwise, and the deposit refuses a harness session
id another member of the bloom already holds.

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
- Session reuse gets a principled trigger (the thread, widened by the edge)
  instead of a global default, with journaled evidence either paying for the
  policy or retiring it. Build-cache reuse gets its own — the member, which is
  what the warm target actually holds — and the working tree gets a third: the
  session, which is the only thing a harness will let it belong to.
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
