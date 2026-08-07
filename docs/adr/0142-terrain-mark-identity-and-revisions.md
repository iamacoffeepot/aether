# ADR-0142: Terrain mark identity and revisions

- **Status:** Accepted (shipped — revisioned terrain marks in `crates/aether-kit-terrain/src/mark/`, covered by `crates/aether-kit-terrain/tests/mark_scenario.rs`)
- **Date:** 2026-07-09

## Context

The kit can paint the world plane stack over `aether.kit.world.*`, but a painted
cell is an address in a lattice, not a re-referenceable *thing*. There is no way
to name a place — "this point", "along this path", "over this area" — and act on
it later. A batch of sibling terrain-authoring surfaces in this release each need
exactly that named handle: picking marks in the viewport, selecting and issuing
semantic commands over them, listing them in a widget, running operators against
them, previewing proposed operations, and exposing task-level authoring tools to
MCP. Without a shared annotation vocabulary each surface either reinvents its own
ad-hoc handle or addresses geometry by raw position.

Position-addressing is the trap. Marks are authored by both people and agents,
and edited afterward — moved, extended, reshaped, often over a TestBench-driven
edit loop. A handle that is "the polygon at these coordinates" stops resolving
the instant the mark is edited, and two authors editing concurrently cannot tell
whose version they hold. The load-bearing requirement is therefore *stable
identity that survives edits*, plus a way for a holder of a reference to detect
that the referent changed under them. This is an identity and optimistic-
concurrency contract shared across six downstream issues, so it is decided once
here rather than re-derived (and inevitably diverging) per surface.

The kit already has positional id tables — regions, water planes, smoothing
profiles — where the id is the caller-chosen 1-based index into a `Vec`, and
`insert_*(id, …)` grows the table at that slot. That pattern fits static,
author-time reference data written whole from a serialized world. It is the
wrong fit for interactively-authored marks, and the difference is the subject of
this decision.

## Decision

Add a `mark` annotation vocabulary to `aether-kit`: point, path, and area marks,
each carrying an **engine-assigned stable id** and a **monotonically-rising
revision**, mutated over an `aether.kit.mark.*` mail surface owned by a mark-book
actor.

- **Geometry.** A mark's shape is one of three cases over the existing
  `WorldPoint` (octimeter XZ) vocabulary — a single point, a polyline path, or a
  polygon-ring area — reusing the world crate's point type and its vertex-count
  ceiling (`MAX_STAMP_VERTICES`) so a mark's ring cannot out-scale a stamp's.

- **Stable id.** Ids are assigned by the mark book, not the caller: a
  monotonically-increasing `MarkId(u32)`, session-scoped, handed back in the
  create reply — the same shape as the substrate's session-scoped texture and
  instrument ids, not the caller-indexed region/water-plane tables. A deleted id
  is **retired, never reused**: the counter only advances, so a stale reference
  to a deleted mark resolves to "gone" rather than silently aliasing a later
  mark that reused the slot.

- **Revision.** Each mark carries a `revision: u32` that starts at 1 on create
  and increments by one on every accepted edit (geometry or label). A holder of
  an `(id, revision)` pair can compare it against the live mark to classify its
  reference: equal revision means unchanged, a higher live revision means the
  mark was edited under them (superseded), and an absent id means it was deleted.
  This is the optimistic-concurrency signal the operator, preview, and selection
  surfaces test before acting on a mark they picked earlier.

- **Ownership and reload.** One mark-book actor (`aether.kit.mark`) owns the
  `MarkId → Mark` store and the `next_id` counter, and services
  `aether.kit.mark.{create,update,delete,list,get}`. Create/update/delete/list/
  get replies carry the assigned id and current revision so the whole contract is
  observable — and TestBench-assertable — without a render surface. Because
  stable identity must also survive a hot swap, the book carries its store *and*
  its `next_id` across `replace_component` via `on_dehydrate`/`on_rehydrate`
  (ADR-0101), so ids stay monotonic and unique across a reload, not just across
  in-session edits.

This issue ships the vocabulary, the store, and the mail surface. The picking,
selection, operator, preview, and MCP surfaces that consume it are separate.

## Consequences

- Six downstream surfaces share one identity contract: they hold a `MarkId`,
  round-trip an `(id, revision)` to detect edits, and never address a mark by
  raw geometry. The contract is decided in one place, so they cannot diverge.
- The retire-never-reuse rule means the `MarkId` space is monotonic and
  append-only within a session; a `u32` counter is ample for interactive
  authoring volumes and there is no id recycling to reason about.
- The revision counter gives cheap optimistic concurrency without locking:
  surfaces compare a held revision rather than coordinating. It does not by
  itself *prevent* a lost update — an operator that wants to refuse a stale edit
  must check the revision and choose to; the contract supplies the signal, not
  the policy.
- Marks are session state, distinct from the serialized world plane stack. This
  ADR does not fold marks into the world save format; whether marks persist to
  disk alongside a world is deferred to whatever surface needs it.
- The mark book is a new non-entry actor in the kit's multi-actor module
  (ADR-0096), added to both `export!` invocations, so the exported kind set and
  its `aether.kinds` section grow by the `aether.kit.mark.*` family.

## Alternatives considered

- **Caller-assigned positional ids (the region/water-plane table pattern).**
  Rejected: it makes the *caller* allocate ids, which two concurrent authors (a
  person and an agent) cannot do without colliding, and the table is written
  whole from a serialized world rather than mutated interactively. It fits
  static reference data, not live authoring.
- **Content-hash ids (id = hash of geometry).** Rejected: the id changes on
  every edit, which is the exact opposite of the requirement — a held reference
  breaks the moment the mark moves. Good for dedup, useless for stable identity.
- **Position-addressing, no id at all.** Rejected: no identity means no
  edit-survival and no concurrency signal — the failure this ADR exists to fix.
- **Stable id but no revision.** Rejected: a holder could tell *which* mark it
  referenced but not whether it had changed, so every consumer would re-fetch and
  diff geometry to detect an edit. The revision counter is the cheap, uniform
  signal that makes stale-reference detection a single integer compare.
- **Reusing deleted ids (free-list).** Rejected: recycling an id lets a stale
  reference silently resolve to an unrelated later mark — a correctness hazard
  that a monotonic counter forecloses for negligible cost.
