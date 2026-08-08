# ADR-0143: Terrain proposal/commit transaction

- **Status:** Accepted (shipped — terrain proposal/commit transactions in `crates/aether-kit-terrain/src/world/proposal.rs`, covered by `crates/aether-kit-terrain/tests/proposal_scenario.rs`)
- **Date:** 2026-07-09

## Context

The `aether.kit.world` component (`crates/aether-kit/src/world/`) authors terrain
by fire-and-forget mutation mail: `set_chunk`, `set_cell_points`,
`set_cell_heights`, `set_region`, and the `stamp_{polygon,disc,hexagon}` shape
kinds each mutate the live `World` in place and invalidate the touched chunk plus
its eight cached apron neighbours, which remesh on the next `Render` stage. There
is no step between "author an edit" and "the world is now that edit" — every send
is a commit.

Terrain geometry quality is the recorded weak point of this authoring surface: a
human or agent proposes an edit and cannot judge the resulting geometry until it
has already replaced the landscape. Recovering the prior state means re-authoring
it. There is no place to compute an edit's result, inspect it, and then keep or
drop it as one atomic decision.

This matters most for machine authoring. The sibling terrain-operator contract
(#2928, brushes/automatons — request → bounded execution → result) and the
revisioned-annotation vocabulary (#2927, stable marks) are both aimed at agents
driving terrain over mail. An agent needs to observe what an operation *would*
produce, separately from the committed landscape, before deciding to apply it —
and a `TestBench` scenario needs to assert on the proposed result without that
result having mutated committed state. The current all-sends-are-commits model
has no seam for either.

Two structural facts of the world model make a clean seam available. `World`
stores chunks as a sparse `BTreeMap<ChunkPos, Box<Chunk>>` — each chunk is a
heap box behind a key. And `mesh_chunk(world: &World, at, styles) -> Vec<DrawTriangle>`
is a pure function of the world plus a chunk position; it reads the target chunk
and a bounded apron into its neighbours and returns geometry with no side effect.

## Decision

Add a **proposal/commit transaction** to the world component: a terrain operation
can be *proposed* — staged and observable — before it is *committed* into the
landscape, with an explicit *discard* to drop it.

**A proposal is a copy-on-write chunk overlay.** Proposing an operation clones
only the chunks the operation touches (`Box<Chunk>` per touched `ChunkPos`),
applies the operation to those clones, and records them in a proposal keyed by a
session-scoped `ProposalId`. Committed `World` state is untouched. The overlay is
bounded to the touched footprint — an operation that edits three chunks stages
three boxes, not a world clone.

**Proposed geometry is produced by the committed mesh path, unchanged.** To mesh
or observe a proposal, the component temporarily installs the proposal's staged
boxes into `World.chunks` (a `mem::swap` per touched chunk — chunks are already
boxes behind map keys), calls the same `mesh_chunk` over the touched chunks and
their apron, then swaps the committed boxes back out. Commit is the same install
without the swap-back: the staged boxes stay, and the touched chunks plus their
apron neighbours invalidate and remesh through the existing cache path. Because
proposed geometry and committed-after-commit geometry both come from `mesh_chunk`
over the identical staged chunks, **what a proposal previews is bit-identical to
what committing it produces** — the observability the seam exists for.

**The transaction is three mail kinds with one reply.** `propose` carries the
operation, stages it, and replies a `propose_result` carrying the `ProposalId`
plus an observable digest of the proposed geometry (touched chunk positions,
triangle count, and the world-space bounds of the changed geometry). Staging is
local and deterministic — no async I/O — so the reply is synchronous.
`commit_proposal { proposal_id }` installs the staged chunks and remeshes;
`discard_proposal { proposal_id }` drops the staged boxes. A proposal may
optionally be made the active preview so `capture_frame` renders the staged
chunks in place of their committed counterparts for visual inspection; toggling
preview never mutates committed state.

**The transaction is agnostic to what produced the staged chunks.** `propose`
wraps a terrain operation; the concrete operation payloads are the existing
mutation kinds today and the operator contract (#2928) as it lands. The
transaction fixes only *how* a staged result coexists with committed state and
*how* it is observed — not what the operation is.

## Consequences

- Terrain authoring gains a propose → inspect → commit/discard lifecycle. An
  agent (or a human via the harness) can compute an edit's geometry, observe it,
  and decide, without the edit having replaced the landscape.
- `TestBench` can assert on a proposed result — read `propose_result`'s digest,
  or capture the active preview — and separately assert that committed state is
  unchanged until `commit_proposal`. Proposed and committed are observable
  independently, which the sibling operator work (#2928) needs for deterministic
  operator-result coverage.
- Memory is bounded to the proposal footprint (touched chunks), not a world
  clone. Multiple proposals may be staged concurrently, each its own overlay.
- The commit path reuses the existing invalidate-and-remesh apron logic, and the
  proposed-geometry path reuses `mesh_chunk` unchanged, so proposed and committed
  geometry cannot diverge by construction.
- New surface to maintain: a proposal store on the actor, three mail kinds plus
  one reply kind, and the swap-in/mesh/swap-out staging helper. Proposal ids are
  session-scoped (they do not survive `replace_component` unless carried through
  dehydrate/rehydrate — out of scope here; a swap drops in-flight proposals).
- The overlay's apron reads resolve each neighbour as "the proposal's staged
  chunk if present, else committed," which the swap-in of all touched chunks
  before meshing provides for free — a staged edit spanning a chunk boundary
  meshes against its own staged neighbour, not the committed one.

## Alternatives considered

- **Shadow `World` clone per proposal** — clone the whole `World`, apply the
  operation, mesh the clone. Correct and simple, but clones every chunk and the
  region/water/smoothing tables per proposal regardless of footprint; the
  copy-on-write overlay bounds cost to the touched chunks and reuses the same
  mesh path.
- **Immutable edit-log with undo/redo** — record every mutation as a reversible
  entry and expose undo. Solves "recover the prior state" but not "observe a
  result before it is committed" — the edit is applied first, then reversed,
  so committed state still flickers through the un-inspected edit. The proposal
  model never touches committed state until commit.
- **Diff-only proposals (store the operation, not the staged chunks)** — stage
  the operation payload and re-apply it on demand. Smaller to store, but every
  observation re-runs the operation and the apron resolution has no materialised
  chunk to swap in; materialising the touched chunks once at propose time is
  simpler and makes the observe path a pure `mesh_chunk` call.
- **A second render layer for previews, no transaction** — render an un-committed
  edit as an overlay tint with no propose/commit kinds. Gives visual preview but
  no atomic keep/drop decision and no machine-observable digest, and leaves the
  "author is a commit" model in place underneath.
