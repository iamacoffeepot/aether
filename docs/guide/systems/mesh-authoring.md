# Mesh authoring

Aether's native authoring representation is a small s-expression DSL, not a
triangle-editing model. `aether-mesh` parses the text into a typed tree,
evaluates primitives and transforms into fixed-point polygons, cleans the
polygon stream, and tessellates only where a triangle consumer needs it. The
`aether.kit.mesh` guest actor is the file-backed viewer: it reads a `.dsl` or
minimal `.obj`, atomically replaces its cached `DrawTriangle`s on success, and
replays them on every `Render` stage.

## Decision map

| ADR | Status | What still governs |
|---|---|---|
| [ADR-0026](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0026-primitive-composition-dsl.md) | Accepted | primitive-composition, palette indices, s-expression data |
| [ADR-0051](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0051-dsl-mesh-vocabulary-v1-pinning.md) | Accepted | structural syntax plus torus and sweep |
| [ADR-0052](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0052-mesh-editor-is-the-dsl.md) | Accepted | edit text and reload; no retained vertex/face editor state |
| [ADR-0053](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0053-promote-dsl-mesh-spike-to-crate.md) | Accepted | library-only mesher boundary |
| [ADR-0054](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0054-csg-operators-for-the-dsl-mesh.md) | **Superseded by ADR-0062** | historical BSP/CSG rationale only |
| [ADR-0055](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0055-post-csg-mesh-cleanup-pipeline.md) | **Superseded by ADR-0062** | historical origin; reusable cleanup code was retained |
| [ADR-0056](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0056-cdt-replaces-ear-clipping.md) | Accepted | constrained Delaunay tessellation for non-convex faces and holes |
| [ADR-0057](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0057-canonical-mesh-form-is-ngon-polygons.md) | Accepted | n-gon polygons, not triangles, are the canonical face form |
| [ADR-0062](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0062-retire-csg-from-v1-mesh-dsl.md) | Accepted | no `union`, `intersection`, or `difference` in v1 |

ADR-0026 said conventional imports would not be supported. Current code is
narrower in ambition but broader in practice: `MeshViewer` accepts a minimal
Wavefront OBJ subset in addition to the native DSL. Treat that as a viewer
compatibility path, not a second structural authoring model; OBJ becomes a
flat triangle cache and cannot round-trip through the DSL AST.

## Mental model

Keep the layers distinct:

1. **Text data** is parsed by `aether_mesh::parse` into `Node`. It is Lisp-like
   syntax but never evaluated as code.
2. **The AST** retains primitives, transforms, structural repetition, and a
   palette index per primitive. `serialize` emits a canonical s-expression;
   original whitespace and comments are not retained.
3. **Canonical geometry** is `Polygon`: a fixed-point CCW outer loop, zero or
   more CW hole loops, a face-normal hint, and one `u32` color index.
4. **Wire/render geometry** is triangles. `tessellate_polygon` converts a
   canonical face at the upload boundary; `mesh` offers a direct triangle
   convenience path.
5. **The viewer actor** maps color indices through its eight-entry RGB palette,
   adds polygon-edge outline strips for DSL faces, caches `DrawTriangle`s, and
   resends them to `aether.render` each frame.

The library has no renderer or mailbox dependency. The viewer is guest code in
the multi-actor `aether-kit-commons` wasm module. Its selector is
`aether_kit_commons@aether.kit.mesh`; a bare `aether-kit-commons` load selects the console, not
the mesh actor. Export membership is in
[`aether-kit-commons/src/lib.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit-commons/src/lib.rs).

## DSL vocabulary

Every primitive has exact positional arity and requires `:color <u32>`.
Structural nodes do not take color; child primitives carry it.

| Family | Forms |
|---|---|
| solids | `box`, `cylinder`, `cone`, `wedge`, `sphere` |
| profile/path | `lathe`, `extrude`, `torus`, `sweep` |
| structure | `composition`, `translate`, `rotate`, `scale`, `mirror`, `array` |

The typed variants and their fields are the authority in
[`ast.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/ast.rs); exact arity, keyword, and
error behavior is in
[`parse.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/parse.rs). A representative model
is:

```lisp
(composition
  (box 2 0.5 1 :color 5)
  (translate (0 0.75 0)
    (array 3 (0.6 0 0)
      (cylinder 0.15 1 8 :color 3))))
```

One input contains exactly one top-level form. Unknown heads and keywords,
wrong arity, dotted lists, missing color, malformed vectors/profiles, and
trailing forms are parse errors. `composition` may contain any number of
children. Vectors are ordered `(x y z)`. Rotation uses an axis vector and an
angle passed to the math layer in radians. `mirror` accepts only the symbols
`x`, `y`, or `z`.

`sweep` takes a 2D profile and a 3D waypoint path. Optional `:scales` must have
exactly one scalar per waypoint. It is capped by default; `:open true` omits
the end caps. The mesher transports a frame along the path and stitches
adjacent rings. Boolean-looking heads are not parked syntax: they are unknown
nodes and fail parsing.

## Public library operations

The crate-root exports are intentionally small:

| Rust operation | Purpose |
|---|---|
| `parse(&str)` | DSL text to typed `Node`, or `ParseError` |
| `serialize(&Node)` | canonical DSL text; parse/serialize/parse preserves the tree |
| `mesh_polygons(&Node)` | evaluate to canonical n-gon `Polygon`s |
| `tessellate_polygon(&Polygon)` | display-time fixed-point triangles |
| `mesh(&Node)` | convenience pipeline directly to colored float `Triangle`s |
| `to_obj(&[Triangle])` | inspection export grouped by palette index |
| `surface_net(...)` | separate dense-scalar-volume boundary extractor |

See [`lib.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/lib.rs),
[`polygon.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/polygon.rs), and
[`surface_net.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/surface_net.rs). `Node`,
`Polygon`, `Point3`, and `Triangle` are library types, not mail kinds.

The geometry pipeline simplifies identity transforms, emits primitive n-gons,
then runs exact-grid cleanup: weld vertices, repair T-junctions to a fixed
point, merge coplanar same-color boundaries, and remove short-edge slivers.
The public polygon path groups boundary loops deterministically into outers and
holes. The triangle path runs cleanup followed by CDT. The implementation is in
[`mesh.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/mesh.rs),
[`cleanup.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/cleanup.rs), and
[`tessellate.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/tessellate.rs).

Convex faces without holes use a fan fast path. Concave faces or faces with
holes use integer constrained Delaunay triangulation. If CDT fails, the public
display helper logs a warning and fan-triangulates the outer and each hole
independently. That keeps geometry visible but fills holes incorrectly; a CDT
warning is a correctness signal, not harmless noise.

## Viewer mail surface

The request and result intentionally use different kind prefixes:

| Mail kind | Rust payload | Meaning |
|---|---|---|
| `aether.kit.mesh.load` | `aether_kit_commons::mesh::LoadMesh` | read and load `namespace` + relative `path` |
| `aether.mesh.load_result` | `aether_kinds::MeshLoadResult` | echoes path, `ok`, optional error, and warnings |

Send the request to the loaded actor mailbox `aether.kit.mesh`. Although an
older kind comment calls it fire-and-forget, the current handler preserves an
available reply target and returns `MeshLoadResult` after fs read and parse
settle. Await the reply when automation needs a definitive outcome.

Extension matching is case-insensitive. DSL input must be UTF-8; OBJ is read
directly from bytes and validates the recognized numeric tokens:

- `.dsl` uses `parse` → `mesh_polygons` → `tessellate_polygon`. Filled faces
  use `color % 8` in the viewer palette; every outer and hole edge also emits a
  narrow lifted slate outline.
- `.obj` accepts positive one-based and relative negative `v` position indices
  in `f` faces, including slash-form references. Faces are fan-triangulated.
  Normals, UVs, groups, materials, smoothing, and other directives are ignored;
  every face is soft blue and receives no outline. Malformed coordinates or
  face indices and references outside the positions defined so far are errors.

Any read, DSL UTF-8, extension, parse, mesh, or OBJ-index failure leaves the
prior triangle cache untouched and returns `ok: false`. The current loader does
not produce non-fatal warnings, though the reply reserves that vector. The
actor and request kind are in
[`aether-kit-commons/src/mesh/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit-commons/src/mesh/mod.rs),
[`mesh/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kit-commons/src/mesh/kinds.rs).
The shared importer is in
[`aether-mesh/src/obj.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/obj.rs);
the shared reply type is in
[`aether-kinds/src/lib.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kinds/src/lib.rs).

## Geometry invariants and failure modes

- Every meshed coordinate is snapped to a signed 16:16 grid. Inputs must be
  finite and remain within inclusive `±256` **after transforms**. Anything
  outside that asset-local range returns `MeshError::OutOfRange`; put
  world-scale placement outside the authored mesh. See
  [`fixed.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mesh/src/fixed.rs).
- Primitive faces are wound CCW from outside. Mirroring reverses vertices to
  preserve outward winding. Degenerate faces can collapse and be skipped
  rather than producing an error.
- N-gon outer loops are CCW and holes are CW around `plane_normal`. Convert
  `Point3` to float only at the render/upload boundary when topology matters.
- Cleanup stage invariant checks currently log warnings rather than aborting.
  A non-simple loop, surviving twin edge, or unrepaired T-junction in logs
  means the canonical output may be damaged.
- The parser and evaluator have no authored-complexity budget or nesting-depth
  cap. Large segment/subdivision/array counts or deeply nested trees can spend
  substantial CPU, memory, or stack. Validate or bound generated input at the
  producer when it is not trusted.
- `serialize` assumes a valid finite in-memory AST and panics if asked to print
  a non-finite float. Parsed non-finite geometry is rejected later at the
  fixed-point boundary.

## Lifecycle and chassis caveats

`MeshViewer` subscribes only to the `Render` lifecycle stage. It performs file
I/O and replaces the cache when a load reply arrives, then resends the cache on
each render stage under the latest view-projection matrix. A chassis without
`Render`, such as the production headless Tick-only graph, rejects the
subscription; the actor can load but never submits geometry. The minimal hub
does not host guest gameplay actors. Use desktop or the render-capable
SubstrateHarness for visual verification. See [Lifecycle](lifecycle.md) and
[Rendering & camera](rendering.md).

## Where to change or extend it

- A new DSL form must update `Node`, parser, serializer, mesher, round-trip
  tests, and reference docs together. If it changes the v1 vocabulary, amend
  ADR-0051 or add a superseding decision.
- Reintroducing booleans is not a parser-only change. It must explicitly
  supersede ADR-0062 and supply topology, resource bounds, native/wasm parity,
  and adversarial geometry coverage.
- Change canonical topology or tessellation in `aether-mesh`, keeping n-gons
  authoritative per ADR-0057. Do not bake viewer palette or render types into
  the library.
- Add a viewer file format in `aether-kit-commons/src/mesh`, preserving whole-cache
  atomic replacement and a structured `MeshLoadResult`. A compatibility
  importer should not silently become a new native authoring representation.
- The editable loop remains file-first: write DSL through `aether.fs`, send
  `aether.kit.mesh.load`, inspect the result, and capture a frame. There is no
  current `set_text` editor mail or retained face/vertex mutation API.
