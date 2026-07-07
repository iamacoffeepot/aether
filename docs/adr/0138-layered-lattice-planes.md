# ADR-0138: Layered lattice planes

- **Status:** Proposed
- **Date:** 2026-07-07

## Context

The kit world (`crates/aether-kit/src/world.rs`) stores a chunk as a stack of dense per-cell planes over a 16×16 cell grid — `underlay` / `overlay` / `height` / `region` / `water_plane` / `smoothing`, plus the per-subcell `underlay_points` and `height_points` planes at `SUB = 4` (16 subcell points per cell, 4096 per chunk). Every height accessor — `World::height`, `point_height`, `surface_level`, `surface_height` — resolves one elevation per `(x, z)` footprint. The height field is total over the 2D lattice: one footprint carries exactly one standable surface.

That is a dimensional limit, not a resolution gap. Two surfaces at the same `(x, z)` — an overhang, an arch, a cave mouth, a walkable bridge over a passage — cannot be expressed no matter how fine the point lattice becomes, because the field has no stacking axis. Per-point relief (the sibling per-point-heights work) sharpens a single surface below cell scale; it does not let a footprint hold a second surface above the first.

The world must gain a stacking axis while keeping the existing surface exactly as it is: the current single-plane world is the overwhelmingly common case and must pay nothing for a feature it does not use. The save format is versioned and append-only (`WORLD_FORMAT_VERSION`, currently 6; older buffers decode by reading defaulted trailing planes), and the live authoring path is the schema-driven `aether.kit.world.set_chunk` wire kind — both are load-bearing, persisted, and cross-process, so the shape chosen here is hard to revise later.

This ADR fixes the *representation* — storage, save format, wire surface, accessors — and records the direction for the meshing, locomotion, and connection work that consumes it. Those are sequenced as follow-on rungs; the closure-pass wall meshing (issue #2713) is a prerequisite for the meshing rung, because the vertical partner search this design adds is an extension of that pass's edge walk.

## Decision

### Plane 0 is the existing chunk; extra planes are additive and sparse

A chunk keeps its current planes unchanged as **plane 0** — the base lattice, byte-identical to today. A chunk additionally carries an ordered, normally-empty list of **extra planes**, each a second (third, …) lattice surface over a rectangular *window* of the chunk's cell grid:

```
struct Chunk {
    // ... every existing plane-0 field, unchanged ...
    extra_planes: Vec<ExtraPlane>,   // empty for a single-surface chunk
}

struct ExtraPlane {
    // window over the chunk cell grid (cells 0..16 on each axis)
    origin_x: u8, origin_z: u8,      // top-left cell of the window
    width: u8, height_cells: u8,     // window extent in cells (1..=16)
    // dense planes over the window's `width * height_cells` cells,
    // same vocabulary as plane 0, row-major within the window:
    underlay:        Vec<Material>,  // Void cell = absent from this plane's silhouette
    underlay_points: Vec<u8>,        // SUB² per cell; Void point carves a hole
    height:          Vec<i32>,       // octimeter elevation per window cell
    height_points:   Vec<i16>,       // SUB² relief deltas per window cell
    region:          Vec<u16>,
    smoothing:       Vec<u8>,
    overlay:         Vec<Material>,
    overlay_mask:    Vec<u16>,
    water_plane:     Vec<u16>,
}
```

The window bounds storage to the extra surface's footprint rather than the whole chunk. The plane's **silhouette** — which cells it actually occupies — is delimited *inside* the window by `Material::Void`, reusing the existing void-point sentinel semantics (an authored `Void` underlay point already cuts a hole; the same convention says a window cell that is Void is not part of this plane). No separate occupancy mask: void is the delimiter, consistent with issue #2713's reading that void marks a jump in geometry to close over.

An extra plane carries the full plane-0 vocabulary so authoring, meshing, and locomotion treat any plane uniformly — a bridge deck is just a plane with its own material, region, and relief.

### Save format v7 — append an extra-plane list per chunk

`WORLD_FORMAT_VERSION` goes to 7. After a chunk record's existing `height_points` plane, v7 writes a `u16` extra-plane count followed by that many extra-plane records (window header `u8 origin_x, origin_z, width, height_cells`, then each dense window plane little-endian). A pre-7 buffer carries no count and no records — it decodes as zero extra planes, exactly as pre-6 decodes as all-zero `height_points`. Decode stays strictly version-gated (`WORLD_FORMAT_VERSION_MIN..=WORLD_FORMAT_VERSION`), truncation returns `Err`, and the caller keeps its prior world on any error.

Migration cost is a single `u16` per chunk (a `0` count) once a v6 world is re-saved as v7, and the version bump is the whole migration. A world with no extra planes meshes byte-identically because the extra-plane loop is empty; **the plane-0-identical guarantee is structural, not asserted after the fact.**

### Wire surface — grow `SetChunk`

`aether.kit.world.set_chunk` gains an `extra_planes: Vec<ExtraPlaneWire>` field (empty by default), each entry mirroring the window header plus the dense window planes as wire-typed vectors, the same way `SetChunk` already carries plane-0's planes and grew once before to add `height_points`. An empty vector is the single-plane default, so an existing sender is unaffected. This is a wire-format change (a new schema field); it rides the established additive-field pattern.

A dedicated incremental authoring kind (`set_chunk_plane` — write/replace one extra plane on one chunk without rewriting the whole chunk, mirroring how `set_cell_points` / `set_cell_heights` are single-cell counterparts to `set_chunk`) is **deferred to a follow-on rung**; the whole-chunk `SetChunk` growth is enough to load and round-trip a multi-plane world.

### Accessors — plane-indexed, plane 0 the default

Height accessors gain a plane-aware form: `point_height_on(plane, cell, sub_x, sub_z)`, `plane_extent(chunk, plane)`, `plane_count(chunk)`, and a footprint query returning the planes present at a cell (plane 0 always, plus any extra plane whose window covers the cell and whose silhouette is non-Void there). The existing signatures (`height`, `point_height`, `surface_height`, …) stay and resolve plane 0 — so today's callers, the mesher's plane-0 pass, and locomotion are unchanged until their rungs land.

### Meshing, locomotion, connections — direction only (follow-on rungs)

- **Meshing across planes.** Built on issue #2713's closure pass: the cap walk records each side's committed edge height, and walls generate where sides disagree. The vertical extension is that a rim's partner search, having found no same-plane partner, searches the plane stack for the nearest surface *below* at that footprint — "below you and above you depending on reference" — and closes the wall down to it. An extra plane's underside is capped so an arch reads solid from beneath.
- **Locomotion plane membership.** The mover gains a current-plane index; `surface_height` becomes plane-scoped (resolve the plane the mover stands on, not the topmost). A mover under a bridge stays on plane 0; a mover on the deck is on the deck's plane.
- **Connections.** Authored edges between planes — walkable transitions (a ramp/stair cell that hands the mover from one plane to another) or teleports (a portal cell pair). Stored as a per-world or per-chunk connection table; a new authoring kind writes them.

### Open product knobs (deferred, not blocking the representation)

These are gameplay/visual calls the follow-on rungs surface; none change the storage shape decided here, so they are recorded rather than resolved now:

- Whether an extra plane's underside renders at all, and in what material (its region cliff material, a dedicated underside material, or none).
- Teleport connection semantics (instantaneous vs. animated, one-way vs. bidirectional, fade/trigger).
- Walkable-connection authoring surface (an explicit ramp cell vs. inferred from overlapping plane rims within the step ceiling).

## Consequences

- A single-surface world is unchanged end to end: same in-memory plane-0 fields, byte-identical mesh, and a save cost of one `u16` per chunk after re-save. The common case pays effectively nothing.
- Overhangs, arches, cave mouths, and walkable bridges become representable and persistent. Storage scales with the extra surface's footprint, not the chunk: a 4×4-cell bridge deck stores ≈ 0.96 KiB and a 2×2 arch ≈ 0.25 KiB, versus ≈ 15.25 KiB for a full second plane per chunk (see Alternatives) — a 15×–60× saving on compact overhangs.
- The window + void-silhouette encoding reuses machinery that already exists (the void-point sentinel, the append-and-version-gate save pattern, the additive `SetChunk` field), so the first rung is storage + format + accessors with no new meshing or locomotion code.
- The stack creates real follow-on work — mesh closure across planes (gated on #2713), locomotion plane membership, and a connection model with its own authoring kind — and forecloses nothing about how those resolve; the accessors are plane-indexed from the start so those rungs extend rather than rewrite.
- The window is a rectangle: a thin diagonal or scattered silhouette inflates the enclosing rect toward whole-chunk cost. This is acceptable for the compact-overhang common case; a per-cell sparse encoding is the escape hatch if scattered planes become common (see Alternatives).

## Alternatives considered

- **A full dense extra plane per layer per chunk.** Simplest, but every extra plane costs the full plane-0 chunk-record footprint regardless of coverage — `underlay 256 + overlay 256 + overlay_mask 512 + height 1024 + water_plane 512 + region 512 + smoothing 256 + underlay_points 4096 + height_points 8192 ≈ 15.25 KiB` even for an overhang covering four cells. Rejected: a stacking axis is normally sparse, and paying 15 KiB for a few occupied cells is the waste the window is designed to avoid.
- **A per-cell sparse plane (a list of `{cell_index, payload}` for occupied cells only).** Payload per cell ≈ 61 bytes (height 4 + height_points 32 + underlay 1 + underlay_points 16 + region 2 + smoothing 1 + overlay 1 + overlay_mask 2 + water_plane 2) plus a 1-byte cell key = 62 bytes/cell. For a compact 4×4 blob (992 B) it is within ~1 % of the window (982 B) but meshes less directly (no dense rect to iterate); its win is scattered silhouettes — 16 cells spread across a 12×12 enclosing rect cost ≈ 8.8 KiB as a window but ≈ 0.99 KiB per-cell. Deferred rather than rejected: it is the drop-in replacement for the window's dense body if a real scattered-plane authoring case appears, and the plane-indexed accessors do not care which backs the plane.
- **A new per-plane wire kind instead of growing `SetChunk`.** Cleaner separation and incremental authoring, but a serialized multi-plane world still needs to round-trip through one whole-chunk write. Growing `SetChunk` (its established additive-field pattern) covers load/round-trip now; the incremental kind is kept as a follow-on for live authoring, not a substitute for the bulk path.
- **A ceiling/roof height field on the existing plane (a second scalar per cell).** Would express a simple overhang cheaply but cannot represent a *standable* second surface with its own material, relief, and region, nor a stack of more than two. Rejected: the requirement is walkable planes and authored connections between them, which is a plane, not a scalar.
