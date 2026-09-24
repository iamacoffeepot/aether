# ADR-0236: Imported Character Model Ingestion

- **Status:** Proposed
- **Date:** 2026-09-24

## Context

A standalone character creator needs a recognizably human head, coherent facial
deformation, and controls whose results survive beyond one live-engine session.
The primitive-composition prototype established the interaction and framing,
but building the eyelids, lips, nose, and jaw as separately positioned
primitives puts a low ceiling on anatomy. It also makes intersections a control
problem: a mouth-depth change can push the mouth through the face instead of
changing one continuous surface.

ADR-0026 currently says that every engine mesh is authored in the
primitive-composition DSL and explicitly rejects conventional mesh import. That
decision protects Aether's generation-first default, but it prevents a
file-backed product actor from consuming a character authored in an ordinary 3D
tool. ADR-0025 separately rejects making PBR materials, normal maps, and a broad
asset pipeline part of the core renderer. Imported character ingestion must
therefore be a narrow exception at the application boundary, not a new default
asset system or a fidelity mandate for the substrate.

The current renderer already supplies the mechanisms an application actor
needs without a character-specific native capability. ADR-0171's
`CreateGeometry` and `UpdateGeometry` store indexed vertex data, while
ADR-0170's `ProgramRegister` and `ProgramDispatch` execute actor-authored vertex
and fragment stages. The geometry layout can carry positions, normals, texture
coordinates, joint indices, and weights. ADR-0170's `CreateTexture` registers
decoded image pixels, and ADR-0140's `DrawMaterialTextured` can composite a
writable program output. The file capability's `aether_fs::Read` gives a
component bytes from a configured namespace without granting it a host path.

`aether-puppet` demonstrated OBJ ingestion and deformation, but it was archived
off `main` in #6654 and is not a supported dependency or product surface. The
new character creator must own only the smaller reusable pieces it needs and
must not restore the archived actor wholesale.

## Decision

Permit conventional imported meshes for explicitly configured application
actors while keeping the primitive-composition DSL as Aether's native and
generation-first mesh representation. This narrows, rather than replaces,
ADR-0026's prohibition: the substrate and mesh kit do not gain an ambient model
importer, and loading an imported model is never an implicit fallback for DSL
content.

The first supported character interchange is a constrained glTF 2.0 binary
file (`.glb`). A character-creator component reads one configured GLB through
`aether_fs::Read`, validates the supported subset, and converts each mesh
primitive into `CreateGeometry` bytes. Version one accepts one default scene,
its acyclic node hierarchy and transforms, triangle primitives with
32-bit-addressable indices, positions, normals, texture coordinates, embedded
PNG or JPEG base-colour images, base-colour factors, and position/normal morph
targets. It rejects unsupported or ambiguous content with a load result that
names the offending scene, node, primitive, accessor, image, or material; it
does not silently drop required attributes. External buffers, external images,
skins, animations, cameras, lights, sparse accessors, compression extensions,
and a general metallic-roughness PBR interpretation are outside the first
version.

Facial controls are config-named glTF morph targets. Every target the mapping
references must have a unique name in the mesh's `extras.targetNames`; a
missing or duplicate name is a load error rather than an invitation to depend
on accessor order. The component retains the neutral positions and normals plus
their morph deltas, evaluates the weighted sum when a slider changes, and
replaces the affected resident geometry with `UpdateGeometry`. Geometry is not
re-uploaded merely to redraw an unchanged face. A slider configuration maps a
stable public control name to one or more morph-target names, ranges, and
coefficients. Mouth, lip, jaw, nose, eye, and brow changes deform authored
topology; the component does not synthesize a second facial shell or translate
an overlay toward the head.

The creator uses the existing widget component rather than drawing bespoke
controls. A `WidgetPanel` lays out `SliderConfig` children beside the preview,
and source-attributed `SliderChanged` mail selects the stable control mapping.
Streaming changes preview the face during a drag; the final `committed` change
is the durable edit boundary for any later persistence feature.

The character component authors a draw program over the imported geometry and
renders into a writable texture. Version one uses normals, base colour, and a
small key/fill/rim lighting model chosen by the component. This is deliberately
not a substrate PBR material system. Multiple GLB primitives may represent
skin, eyes, hair, and clothing; the component registers the draw-pass graph
needed for the validated primitive count and composites its result with the
ordinary material/overlay surface.

The shipped creator is durable as source plus assets: its component crate,
WGSL, GLB, slider mapping, component init config, and boot/depot manifest are
checked-in or packaged together. Init config names the file namespace, GLB
path, slider-map path, and presentation defaults. Live slider values remain
ordinary actor state; saving named characters is a separate persistence
decision.

## Consequences

- Aether can present a substantially more realistic head without putting a
  character model, facial vocabulary, or PBR policy in the substrate.
- Facial sliders operate on artist-authored deformation, which preserves mesh
  continuity and removes the primitive prototype's mouth/face intersection
  class.
- GLB keeps the first asset transaction to one file and makes the namespace
  boundary deterministic. Supporting loose `.gltf`, external images, or
  extension-compressed data requires a later decision or extension of this one.
- The component pays a CPU morph-and-upload cost when controls change. That is
  acceptable at UI cadence and avoids consuming a vertex attribute per morph
  target. Continuous animation should instead use resident attributes and
  shader uniforms or a later deformation resource.
- The creator's stylized lighting can improve form and material separation, but
  the result will not match an engine with a full PBR stack, shadows, subsurface
  scattering, or modern global illumination. This is an intentional limit from
  ADR-0025, not an importer defect.
- Imported assets become trusted package inputs with explicit provenance and
  licence obligations. The repository or package must record the source and
  licence of every shipped model and texture; the live engine does not fetch
  models from arbitrary URLs.
- Follow-on implementation needs a small GLB decoder boundary, validation and
  malformed-input tests, fixture assets that are redistributable, a
  file-configured creator actor, widget bindings for the named sliders, and a
  live capture proving neutral and extreme control states.

## Alternatives considered

- **Continue composing the head from DSL primitives.** Rejected for this
  product: it cannot reach the requested anatomy and makes facial coherence a
  collection of intersection fixes.
- **Restore `aether-puppet`.** Rejected: it was intentionally archived, carried
  a much larger pen-plotter product, and is not the supported reusable importer
  boundary.
- **Add glTF ingestion to the substrate or mesh kit.** Rejected: it makes
  conventional imports ambient engine policy and reopens ADR-0026 for every
  consumer instead of defining one explicit application boundary.
- **Start with OBJ plus sidecar deformation files.** Rejected: OBJ has no
  standard morph-target, hierarchy, or embedded-texture vocabulary, so the
  sidecars would become an Aether-specific character format before one imported
  character ships.
- **Implement full glTF PBR and animation in version one.** Rejected: it
  conflicts with ADR-0025, multiplies the validation surface, and is unnecessary
  for a head creator whose first controls are static morph weights.
- **Evaluate every morph target in the vertex shader.** Deferred: it avoids
  slider-time uploads but fixes target count into the geometry/program layout
  and spends vertex bandwidth every frame. CPU evaluation is simpler at UI
  cadence; continuous animation can justify a separate design.
