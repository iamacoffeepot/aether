# ADR-0026: Primitive-composition DSL as native mesh representation

- **Status:** Accepted
- **Date:** 2026-04-19

## Context

ADR-0025 bounds the renderer at a chunky low-poly flat-shaded aesthetic with palette-indexed per-vertex color. That choice is deliberate: it makes the engine's mesh format trivially machine-generable, which is the lever the project needs to pull for its generation-first product positioning.

The product positioning matters here. Aether is not primarily a graphics engine. It is an LLM-driven worldbuilding engine that happens to render. The renderer exists to make generated content feel alive enough that the generation loop is satisfying. The **representation** of content — the format in which models enter the engine — is therefore a higher-leverage design question than any rendering feature, because it determines what an LLM (or any automated producer) can actually author.

The conventional mesh-authoring path is hostile to this positioning. Industry-standard asset formats (glTF, FBX, OBJ, USD) are exports of interactive 3D modelling tools. They encode a finished triangle soup plus skinning data plus material references, with no structural prior that would constrain style. A model author uses a modelling tool; the tool emits the format. LLMs are not good at emitting these formats directly (the representation is too low-level — triangle coordinates, weight maps, material bindings — and too unconstrained to enforce stylistic consistency). Trying to use LLMs as drop-in replacements for modelling tools is a well-documented failure mode.

The alternative is to commit to a mesh representation that is itself structured for machine generation: a small, grammar-constrained DSL that describes models parametrically, where style constraints are structural rather than attentional (i.e. enforced by the grammar, not by "please follow the style guide"). The target aesthetic from ADR-0025 is native to this approach — chunky low-poly flat-shaded geometry is primitive-composable by construction. A character is a tree of boxes, cylinders, and wedges. An item is a lathed 2D profile plus a handle box. A building is a stack of extruded floor shapes. None of these require a triangle-level description to represent.

This ADR commits the engine to that approach as the **only** path by which mesh content enters the engine. No conventional asset import. No modelling-tool-exported meshes. Mesh content is authored as DSL, parsed, and meshed at load time.

The scope is explicitly mesh representation. Terrain (likely heightmap), particles (likely parametric), skybox (likely shader), UI (likely conventional 2D atlases), and textures-for-specific-reasons are all out of scope for this ADR and will be addressed separately if and when they surface.

## Decision

**Every mesh in the engine is authored as a text-based primitive-composition DSL, parsed into a tree of primitives with transforms and palette-indexed colors, and meshed to triangles at load time. Conventional mesh asset import (glTF, FBX, OBJ, USD) is not supported.**

### Representation model

A mesh is a tree. Each node in the tree is one of:

- A **primitive**: a parameterised 3D shape from the primitive vocabulary (below).
- A **transform**: a translation / rotation / scale applied to a child subtree.
- A **composition**: a group node containing multiple children, each with its own local transform.
- A **structural operator**: built-in operators that reduce LLM authoring complexity — at minimum `mirror(axis, subtree)` for symmetric objects and `array(n, spacing, subtree)` for repeated elements.

Every leaf primitive carries a **palette index** referring to the enclosing scene's declared palette. RGB colors are not used directly. Off-palette colors are not representable.

Primitive vocabulary v1 (committed by this ADR):

1. `box(x, y, z)` — axis-aligned rectangular solid.
2. `cylinder(radius, height, segments)` — prismatic cylinder; `segments` defaults low (e.g. 8) to match the chunky aesthetic.
3. `cone(radius, height, segments)` — truncated-cone base parameter omitted for v1; straight cone only.
4. `wedge(x, y, z)` — right-triangular prism; the workhorse for roof shapes, weapon blades, and non-axis-aligned chunky forms.
5. `sphere(radius, subdivisions)` — icosphere; `subdivisions` defaults low.

Profile operations v1 (committed by this ADR):

6. `lathe(profile, segments)` — revolve a 2D profile around the Y axis.
7. `extrude(profile, depth)` — extrude a 2D profile along Z.

A 2D profile is a list of `(x, y)` points. Profiles are literal lists in the DSL; they are not a separate asset type.

Explicit non-members of the v1 vocabulary:

- **CSG boolean operators** (union, intersection, difference). Not implemented. Overlap-by-transform handles the common case (two primitives of the same palette index visually read as one shape), and CSG implementation cost is non-trivial. Deferred until demand shows up.
- **Sweep-along-path.** Deferred; not needed for the initial demo set.
- **Signed distance function composition, metaballs, subdivision surfaces.** Forecloseed by ADR-0025 (wrong aesthetic — produces smooth organic shapes that violate the faceted silhouette).

### Color and palette

- A mesh declares or references a **palette**: an ordered list of RGB colors.
- Every primitive's color is a palette index, not an RGB triple.
- Palettes may be declared at the model level (self-contained), at the scene level (shared across many models), or globally (shared across scenes). The scope choice is a v1 decision point for the implementation; this ADR commits only to the indexing model.
- Per-face color within a primitive is deferred. v1 is one palette index per primitive.

### Animation scoping

Animation is deferred to a separate ADR. This ADR commits only to static mesh representation. However, one property of the chosen representation is worth noting for future-us: because a mesh is a tree of nodes with transforms, animation is naturally expressible as **transform animation on the existing tree** — no separate skeleton concept is required. The tree is the skeleton. This is a deliberate affordance of the representation, not a commitment of this ADR.

### Format

Serialized syntax is **a Lisp-syntactic s-expression data format**. A model is a parenthesized tree; each node is a head symbol (primitive or operator name) followed by parameters and children. Example shape:

```
(composition
  (translate (0 1 0)
    (box 1 1 1 :color 3))
  (mirror x
    (translate (0.5 0 0)
      (cylinder 0.2 1 8 :color 5))))
```

**The format is Lisp-syntactic, but it is data, not a programmable Lisp.** There is no evaluator, no `let` bindings, no lambdas, no macros, no quasiquotation. The parser produces a static tree; the mesher walks it and emits triangles. This distinction matters:

- It lets the engine lean on the full weight of LLM training data for s-expression syntax (comment conventions, numeric literals, string literals, keyword arguments) without also inheriting the semantic surface of a real Lisp.
- It bounds the attack surface. LLM-generated input is parsed as data, never evaluated.
- It keeps the "what subset is supported" question trivial: if it's not in the primitive / operator vocabulary, it is not supported, and that rule is mechanically checkable at parse time.

The format is intentionally compatible with existing Lisp-data readers — specifically the `lexpr` crate's R6RS-flavored reader is the leading candidate for the parser implementation — so the parser itself is a dependency plus glue rather than a from-scratch build.

S-expressions are the right fit for this representation for four reasons:

- **The representation is a tree**, and s-expressions are the syntax with the least overhead between "tree" and "text." Nesting is parentheses; structure is visible at a glance; there is no framing ceremony (no JSON `{"type": ..., "children": [...]}` boilerplate around every node).
- **The grammar is small, regular, and battle-tested.** Reusing Lisp-data conventions means every micro-decision (how do we comment? how do keyword args work? how do numbers parse?) has a conventional answer we can adopt without inventing one.
- **LLM training coverage is enormous.** LLMs have seen vast amounts of Lisp-family code and data. Few-shot priming on a small vocabulary against this baseline produces reliable emission.
- **Edits are localized.** Adding a node, moving a subtree, or wrapping a subtree in a new transform is a structural edit that diffs cleanly. Conventional mesh formats and even JSON accumulate surrounding noise on structural edits; s-expressions do not.

Concrete sub-grammar decisions (exact keyword-argument syntax, comment conventions, numeric literal format) default to the reader library's defaults, with divergences called out only if the spike surfaces friction. The commitment is: Lisp-syntactic s-expression *data format*, parsed via an existing reader, not a programmable Lisp.

The parser must:

- Be round-trippable (parse → AST → serialize → parse produces structurally identical trees).
- Be human-readable and human-editable.
- Be LLM-emittable (grammar-constrained enough to be reliable; vocabulary small enough to fit in few-shot examples).
- Diff cleanly in git (text format; no binary blobs).

### Mesh generation

A DSL mesh is loaded, parsed to an AST, and meshed to GPU-ready vertex and index buffers at load time. Meshing is deterministic: the same DSL input always produces byte-identical vertex buffers. This enables caching, golden-test fixtures, and reproducible rendering for generation-quality measurement.

The mesher lives in the substrate. Components that want to author meshes emit DSL text via mail; the substrate parses, meshes, and retains the result. A component does not ship pre-triangulated data. (The host-fn surface for component-authored meshes is implementation detail, not committed by this ADR; the commitment is that the path is DSL-in, not triangles-in.)

## Consequences

### Positive

- **LLM generation is a first-class path, not a bolted-on feature.** The mesh format is exactly what LLMs are good at emitting: small-vocabulary structured text with constrained grammar. Generation quality is bounded by model capability, not by the representation's hostility to machine authoring.
- **Style consistency is structural.** Off-palette colors cannot be emitted. Non-primitive shapes cannot be emitted. Smooth-surface aesthetics cannot be emitted. The grammar is the style guide.
- **Authoring is text.** All mesh content is git-versionable, diffable, reviewable, search-grep-able. No binary blobs, no merge conflicts on modeling-tool state, no asset-management problem.
- **The representation and the aesthetic reinforce each other.** ADR-0025's chunky low-poly flat-shaded aesthetic is exactly what primitive composition produces naturally. Neither ADR creates tension with the other.
- **Procedural content generation becomes uniform.** LLM-generated meshes and hand-written-code-generated meshes emit the same DSL. There is no "pro path" vs "scripting path" — everything is DSL.
- **The tree-as-skeleton affordance.** Animation, when added, is transform animation on the same tree — no separate skeleton asset, no skin binding, no weight painting. This compresses the whole animation authoring story considerably.
- **Fine-tuning is tractable.** A (description, DSL) pair dataset can be generated by the engine itself: hand-author some reference models, generate permutations, render them, train on the description→DSL mapping. The dataset-generation loop is in-engine.

### Negative

- **No imported assets.** Existing model libraries (glTF marketplaces, free-asset repositories, user-contributed meshes from other engines) cannot be used. Everything must be re-authored as DSL. This is intentional but real.
- **Organic / soft / continuously-deforming shapes are awkward.** Cloth, flowing hair, detailed faces with vertex-blended expressions, fluid surfaces — none of these are native. They can be approximated by combinations of primitives, but that is work the grammar does not help with.
- **The primitive vocabulary bounds the asset space.** Every shape an author wants must decompose into v1 primitives + profile operations. Exotic shapes (e.g. a tree with gnarled branches, a face with subtle muscle structure) require creative decomposition or deferred-vocabulary extensions.
- **The mesher is a non-trivial piece of substrate code.** Each primitive needs deterministic meshing, transform composition, and palette-color propagation to vertex attributes. Not large, but not nothing.
- **Deterministic meshing constrains the implementation.** Caching is a benefit; the constraint is that every meshing-algorithm change is observable as a visible-output change. Golden tests catch regressions but make "minor mesher tweaks" higher-friction than they would otherwise be.
- **LLM generation reliability is gated by DSL design.** The DSL is more important than the model. A poorly-designed DSL with raw transforms everywhere makes LLMs unreliable; a well-designed DSL with `mirror` and `array` and relative-positioning affordances makes them reliable. This ADR commits to the approach but does not guarantee LLM reliability out of the box — the parser/grammar/vocabulary design phase has to land that.

### Neutral

- **Fine-tuning is an option, not a requirement.** General-purpose LLMs can produce acceptable output against a well-designed DSL today; fine-tuning improves reliability further. The architecture does not depend on fine-tuning, but it is compatible with it.
- **The DSL is a user-facing surface.** Developers who want to hand-author a model edit DSL text. That is a reasonable authoring experience for primitive-composed models; it would not scale to conventional mesh authoring, but this ADR explicitly is not trying to serve conventional mesh authoring.
- **Non-mesh content types are out of scope.** Terrain, particles, skybox, UI, audio — none are affected by this ADR. Each will get its own representation decision when surfaced.

## Alternatives considered

- **Support conventional mesh import (glTF, FBX, OBJ).** Rejected: directly contradicts the generation-first positioning. Imported meshes cannot enforce the palette / primitive-vocabulary constraints that make the aesthetic machine-generable. If imports are allowed, the DSL becomes a second-class path rather than the native path, and the structural style-consistency property is lost. Deferred permanently in the current positioning; revisitable if the product positioning changes.
- **Voxel representation (regular grid of palette-indexed cells).** Rejected (also per ADR-0025): grids are LLM-friendly but produce blocky silhouettes, not chunky-continuous-diagonal silhouettes. Different aesthetic; different rendering path; different tool story.
- **Signed distance function composition.** Rejected: produces smooth organic shapes that violate ADR-0025's faceted silhouette requirement. Also expensive to mesh (marching cubes, dual contouring, etc.).
- **Constructive solid geometry (CSG boolean operators as the primary representation).** Rejected as the *primary* representation because boolean ops are expensive to mesh and LLMs do not reason about them reliably. Overlap-by-transform handles the common case for the target aesthetic. CSG operators may be added as a v2 vocabulary extension if a demo needs them.
- **Shape grammar / L-system as the primary authoring format.** Rejected as primary: grammars are excellent for specific content types (vegetation, procedural buildings) but too constraining as a general-purpose mesh format. Nothing prevents a component-level shape grammar from *emitting* DSL; the grammar is a producer, not a representation.
- **Direct triangle authoring via DSL** (list of vertices + indices + colors in text). Rejected: no structural style constraint; the DSL is just a text envelope around conventional mesh data. LLMs emit this unreliably. Defeats the purpose.
- **Hybrid: DSL + imported meshes as two first-class paths.** Rejected: two first-class paths means neither is the path. Style consistency is lost, tooling bifurcates, generation and authoring diverge. If a hybrid is ever needed, it is a supersession of this ADR, not an extension.
- **Defer the ADR; let the parser spike inform the decision.** Rejected: the spike implementation is downstream of this decision, not upstream. The commitment to DSL-only vs. import-supporting is a product-positioning decision, not a parser-complexity decision. Waiting on the spike inverts cause and effect.
- **Embed a full Lisp (Scheme / Clojure-like) with an evaluator.** Rejected: the leverage of Lisp-family syntax for LLM authoring is real, but the semantic surface of an evaluator (let-bindings, lambdas, macros, quasiquotation, tail calls, dynamic scoping rules) is not needed for a static mesh description and introduces meaningful cost. Cost vectors include: implementation and ongoing maintenance burden of the evaluator; security surface from evaluating LLM-generated code; a "which subset of Lisp works?" documentation burden; the risk that LLMs emit valid-looking programs using language features the engine does not support. The chosen path — Lisp-syntactic *data format*, parsed as data, never evaluated — captures the LLM-authoring win without the semantic carry cost. Revisitable if a future use case genuinely needs authored computation (procedural parameterization that cannot be expressed via structural operators), but likely as a separate "scripting" layer above the mesh DSL, not as an expansion of the mesh DSL itself.
- **Invent a bespoke s-expression grammar.** Rejected in favor of reusing an existing Lisp-data reader (e.g. `lexpr`). A bespoke grammar is a small but ongoing maintenance cost for zero product-facing benefit; reusing an established reader also maximizes LLM training-data overlap, which is load-bearing for the generation-first positioning.

## Follow-up work

This ADR commits the representation model. Implementation lands as separate PRs:

- **DSL parser spike.** Integrate `lexpr` (or equivalent) as the reader; define the `serde` / AST mapping from generic s-expression data to typed mesh AST; hand-author 3–5 reference models (a mushroom, a crate, a simple humanoid figure, a lamp post, a tree); verify round-trip and visual output. Sub-grammar choices (which reader dialect, keyword-arg syntax exact form) resolved inside this spike.
- **Mesher v1 in `aether-substrate`.** Deterministic meshing for the v1 primitive vocabulary + lathe/extrude. Golden-test fixtures for each primitive.
- **Palette system.** Scope decision (model / scene / global); storage format; color-space handling (linear vs. sRGB in palette entries).
- **Mesh loading host-fn / mail path.** How a component hands DSL text to the substrate and receives a mesh handle in return. Implementation detail; specifics depend on where the mesher lives.
- **First generation-loop demo.** LLM (via MCP) emits DSL → engine renders → screenshot feedback loop (pending ADR / spike) returns image. This is the milestone that proves the core thesis.
- **Documentation: DSL reference + style guide.** A small authored document describing the primitive vocabulary, structural operators, palette system, and a handful of worked examples. Doubles as the few-shot prompt material for LLM-based authoring.

Parked, not committed:

- **v2 vocabulary extensions.** CSG boolean operators, sweep-along-path, smoothed-edge variants of primitives, custom-profile sphere (ellipsoid / squashed), per-face palette indices. Each added if and when a demo requires it.
- **Animation representation.** Deferred to a separate ADR. The tree-as-skeleton affordance is noted but not committed.
- **Fine-tuning dataset generation pipeline.** Relevant once the DSL stabilizes; premature before the parser and reference-model set exist.
- **Visual DSL editor.** A UI that edits DSL structurally rather than as text. Not needed for v1; text editing is sufficient. Consider later if authoring volume grows.
- **Imported-reference-aided authoring.** A tool that takes an inspiration image and proposes a DSL decomposition. Interesting but speculative; out of scope until the core loop works.
