# World & terrain authoring

> **Decision status:** [ADR-0140](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0140-render-material-pass.md) and
> [ADR-0141](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0141-editor-shell-input-ownership.md) are Accepted;
> ADR-0140's material pass is shipped, and ADR-0141 includes its realized
> editor-shell contract. [ADR-0142](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0142-terrain-mark-identity-and-revisions.md)
> and [ADR-0143](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0143-terrain-proposal-commit-transaction.md) are
> **Proposed**. Their mark and proposal surfaces exist in current code, but
> consumers should not describe those decisions as Accepted.

Terrain is a sparse property lattice, not a collection of entity objects.
`WorldView` owns committed planes and cached render geometry. `MarkBook` names
places independently of those planes. `TerraEditor` owns an ordered selection
and translates semantic commands into mark-store requests. `TerrainWorkbench`
assembles those peers with a viewport, controls, console, and editor-wide input
arbiter.

## Ownership map

| Actor mailbox | Rust actor | Owns |
|---|---|---|
| `aether.kit.world` | `WorldView` | `World`, chunk mesh cache, mark projection, staged proposals |
| `aether.kit.mark` | `MarkBook` | stable mark ids, revisions, geometry, labels |
| `aether.kit.terra` | `TerraEditor` | ordered selection and one in-flight semantic command |
| `aether.kit.workbench` | `TerrainWorkbench` | UI draft, peer coordination, one workbench request, current proposal |

All four are guest actors exported from the multi-actor `aether-kit` module;
they are not chassis capabilities. Load them by selector, for example
`aether_kit@aether.kit.world`, rather than relying on the module's bare-load
entry. See
[`aether-kit/src/lib.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/lib.rs).

## The world plane stack

One cell is one meter and 256 octimeters. Chunks are sparse `16 × 16` cell
blocks, addressed correctly across negative coordinates by arithmetic shift
and Euclidean remainder. Each cell has `16 × 16` subcells for authored
material coverage and relief.

The important planes are:

- **Underlay:** ground material. `Void` falls through to the cell's region
  default; no region/default remains void. Per-subcell underlay points may
  inherit that cascade or explicitly pin a material, including `Void` holes.
- **Overlay:** one placed material per cell plus a byte of scalar coverage per
  subcell. Coverage `128..=255` is inside the rendered surface. Same-material
  stamps max-compose; a different material takes cell ownership and clears the
  prior mask before painting its samples.
- **Height:** one base octimeter height per cell plus signed `i16` subcell
  deltas. Relief participates in meshing and terrain picking; it is not merely
  a visual decal.
- **Region, water-plane, smoothing:** small per-cell ids into world tables.
  Water uses its authored plane level instead of lakebed height. A height break
  strictly above 64 octimeters becomes a cliff.

Material bytes are `0 Void`, `1 Grass`, `2 Dirt`, `3 Stone`, `4 Sand`, and
`5 Water`; unknown bytes degrade to `Void`. Raw `SetChunk` vectors pad or
truncate to their plane sizes, and region/water ids narrow to `u16`. These are
format contracts, not schema enums. The complete model and versioned binary
codec are in
[`world/data.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/world/data.rs).

`WorldView` meshes each resident chunk to flat-color `DrawTriangle`s and keeps
them in a sorted cache. Meshing reads a bounded neighbor apron, so a write
rebuilds the touched chunk and any already-cached neighbor in its 3 × 3 apron.
Every `Render` stage resends the selected committed or preview cache. Despite
ADR-0140's shipped generic material pass, the current world runtime still CPU
marches coverage and emits triangles; it does not upload R8 coverage textures.
See [`world/mesher/`](https://github.com/iamacoffeepot/aether/tree/main/crates/aether-kit/src/world/mesher) and
[Rendering & camera](rendering.md).

## World mail surface

Kind strings are the wire API; Rust names in the middle column are payload
types, not aliases that can be sent on the wire.

| Kind or family | Rust payload/result | Semantics |
|---|---|---|
| `aether.kit.world.set_chunk` | `SetChunk` | replace all planes for one chunk; immediate commit |
| `aether.kit.world.set_cell_points` | `SetCellPoints` | replace one cell's underlay points |
| `aether.kit.world.set_cell_heights` | `SetCellHeights` | replace one cell's relief deltas |
| `aether.kit.world.stamp_polygon`, `.stamp_disc`, `.stamp_hexagon` | `StampPolygon`, `StampDisc`, `StampHexagon` | compact overlay raster mutations |
| `aether.kit.world.apply_brush`, `.run_automaton` | `ApplyBrush`, `RunAutomaton` → `aether.kit.world.operator_result` / `OperatorResult` | bounded immediate operators |
| `aether.kit.world.pick_terrain` | `PickTerrain` → `aether.kit.world.pick_terrain_result` / `PickTerrainResult` | first top-surface hit of a bounded ray |
| `aether.kit.world.set_mark_overlay_visibility` | `SetMarkOverlayVisibility` → matching `_result` | show/hide the cached mark projection |
| `aether.kit.world.set_mark_overlay_selection` | `SetMarkOverlaySelection` → matching `_result` | highlight an exact cached `MarkRef` |
| `aether.kit.world.propose` | `Propose` → `aether.kit.world.proposal_result` / `ProposalResult` | stage one copy-on-write operation |
| `aether.kit.world.commit_proposal`, `.discard_proposal`, `.set_proposal_preview` | corresponding request types → the same `ProposalResult` kind | finish or render staged state |
| `aether.kit.world.set_region` | `SetRegion` | update region table and remesh all cached chunks |
| `aether.kit.world.load` | `WorldLoad` | fs read + atomic decoded-world swap; errors only in logs |

Exact fields, reply variants, and limits are in
[`world/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/world/kinds.rs).
There is no public world-query mail. In automation, observe typed operation /
proposal replies and rendering; `WorldLoad` and raw immediate mutations do not
acknowledge success.

Polygon stamps accept at most 1,024 vertices, 4,096 subcells on either edge,
1,048,576 subcells of raster area, and an estimated 33,554,432 units of
scanline work. Degenerate, oversized, zero-radius, unknown-material, and
`Void` stamps touch nothing and return no error. Concave polygon fill uses the
even-odd rule. The bounds and composition rules are implemented in
[`world/raster.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/world/raster.rs).

## Bounded operators

`ApplyBrush` places disc stamps along a named octimeter path at stable spacing.
`RunAutomaton` currently supports one deterministic four-neighbor `Grow` rule.
Both carry a `MarkRef source` plus maximum steps and subcells. The world actor
does **not** resolve that reference against `MarkBook`; direct callers provide
attribution, not authorization or freshness proof. The workbench performs its
own exact-revision fetch before constructing an operation.

Operators charge before mutation. On a limit, the over-cap write is not made,
but the already accepted prefix remains a consistent **committed** mutation and
is remeshed. `OperatorResult::Failed` reports that prefix's exact steps,
subcells, and sorted touched chunks. Invalid parameters report zero work. Code
lives in
[`world/operator.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/world/operator.rs).

## Proposal lifecycle — Proposed ADR

`Propose` wraps `SetChunk`, point/height writes, all three stamps, or either
operator. It clones only chunks first written by the operation, temporarily
installs all staged boxes to run the ordinary mesher, then restores committed
state. A staged result includes its operation result and a digest: sorted
touched chunks, triangle count across the proposal's affected cached meshes,
and optional bounds containing changed before/after geometry.

Up to 64 proposals can coexist. Each records the current committed revision.
Any later touched immediate mutation or successful commit advances that
revision and clears the active preview, making peer proposals stale. Preview
and commit require freshness; discard accepts fresh or stale ids. An operation
that touches nothing rejects without consuming an id. Proposal ids are
monotonic, session-scoped, and not dehydrated across component replacement.

Preview switches only rendering; committed `World` and its cache remain
unchanged. Commit installs all staged boxes together and remeshes once. Unknown,
stale, exhausted-id, at-capacity, and no-touch cases are typed
`ProposalError`s. See
[`world/proposal.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/world/proposal.rs) and
[`world/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/world/mod.rs).

## Marks and semantic selection — Proposed ADR

`MarkBook` stores point, path, and area annotations over named `WorldPoint`s.
Ids start at 1, are engine-assigned, and are never reused after delete.
Revisions start at 1 and increment on each accepted update. Paths require
2..=1,024 points and areas 3..=1,024. Store and allocation watermark survive a
hot replacement through `SavedMarks`; marks are **not** part of the serialized
world format.

| Mark kind family | Rust types |
|---|---|
| `aether.kit.mark.create` / `_result` | `MarkCreate`, `MarkCreateResult` |
| `aether.kit.mark.update` / `_result` | `MarkUpdate`, `MarkUpdateResult` |
| `aether.kit.mark.delete` / `_result` | `MarkDelete`, `MarkDeleteResult` |
| `aether.kit.mark.get` / `_result` | `MarkGet`, `MarkGetResult` |
| `aether.kit.mark.list` / `_result` | `MarkList`, `MarkListResult` |

`MarkUpdate` names an id but carries no expected revision. The revision is a
staleness signal for consumers; the store itself does not implement a
compare-and-swap update. The store and validation are in
[`mark/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/mark/mod.rs) and
[`mark/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/mark/kinds.rs).

`TerraEditor` validates exact `MarkRef`s and owns an ordered, duplicate-free
selection. Its public commands are `set_selection`, `toggle_selection`,
`clear_selection`, `create_mark`, `move_selection`, `relabel_selection`,
`delete_selection`, and `query`, all under `aether.kit.terra.*`. Commands reply
with `aether.kit.terra.command_result`; query has its own `_query_result`.
Only one command runs at a time, while query remains available.

Move/relabel/delete read-preflight the whole selection before the first write,
then send mutations sequentially. A revision race or later peer failure can
therefore yield `PartiallyApplied` with exact changed/deleted prefixes; the
preflight is not a multi-actor transaction. See
[`terra/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/terra/kinds.rs),
[`terra/selection.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/terra/selection.rs), and
[`terra/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/terra/mod.rs).

## Workbench assembly and input

Load `MarkBook`, `TerraEditor`, and `WorldView` first, then instantiate
`TerrainWorkbench` with their mailbox ids, non-overlapping tool/viewport/
console rectangles, a valid camera, font/theme settings, and initial operator
budgets. On its first `Tick`, the workbench spawns an inline tool panel,
terrain viewport, console, and `EditorShell`. Missing peers, invalid rectangles,
degenerate camera, bad clip range, or an excessive pick distance reject init.

Per ADR-0141, the shell alone subscribes interactive input and routes between
regions. The panel owns focus within its widgets; viewport presses build a ray
from the same camera that publishes the viewport's `ViewProjection`; console
keys stay in the console region. Region-level press ownership and focus are
deterministic, while drawing remains per-region. Another active camera can
still overwrite the renderer's single latest view-projection matrix.

The workbench is deliberately one-flight. A pick can create a point mark or
append a draft path/area; staging re-fetches the selected mark and requires the
exact revision before translating it to a brush or automaton proposal. An
automaton accepts only a point mark; a brush accepts point, path, or area
points. The public external observability kind is
`aether.kit.workbench.query` → `aether.kit.workbench.query_result`; button
intents are child-internal, so agents that do not drive input should use the
mark/terra/world kinds directly.

See [`workbench/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/workbench/kinds.rs),
[`workbench/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/workbench/mod.rs),
[`workbench/panel.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/workbench/panel.rs), and
[`workbench/viewport.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit/src/workbench/viewport.rs).

## Chassis caveats and extension routes

Desktop and render-capable SubstrateHarness provide the lifecycle, input, text, fs,
and render peers needed for the visual workbench. Mark and terra actors are
render-independent and remain useful in nonvisual tests. The production
headless graph has `Tick` but no `Render`: mutations and queries can run, but
`WorldView` and the viewport never submit geometry/camera mail. The minimal hub
does not host the kit actors.

The mark overlay refresh resolves a default-loaded component named
`aether.kit.mark`; configuring a mailbox on `TerraEditor`/workbench does not
change that lookup. Load the authoritative mark book under that name when the
world projection is enabled.

- Change plane meaning, units, material bytes, or persistence in `world/data.rs`
  and migrate the binary version deliberately.
- Change public terrain mail in the owning `kinds.rs`; keep named coordinates
  and units, typed failure results, and mail-kind/Rust-type distinctions.
- Preserve apron invalidation and the shared `mesh_chunk` path when changing
  proposals, so preview and committed geometry cannot drift.
- Treat mark/proposal semantics as Proposed until ADR-0142/0143 are accepted or
  superseded. A code change does not silently advance an ADR's status.
- Moving overlay rendering onto ADR-0140's R8 coverage material touches both
  world preparation and render integration; keep the CPU mesher as the
  deterministic reference until replacement behavior is proven equivalent.
