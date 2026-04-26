# ADR-0052: The mesh editor is the DSL — text + hot reload, not vertex/face state

- **Status:** Accepted
- **Date:** 2026-04-26
- **Supersedes mesh-editor abstraction in:** Spike C (`aether-mesh-editor-component` v0.1) — the cube/cylinder/extrude/translate-vertices stateful editor introduced in PR 235 / 250 / 252 / 253. The crate name stays; the abstraction inside it changes.

## Context

The substrate currently ships an `aether-mesh-editor-component` whose API treats the mesh as a **stateful vertex-and-face graph**: agents send `aether.mesh.set_primitive` to seed a cube or cylinder, then iterate via `aether.mesh.translate_vertices`, `aether.mesh.scale_vertices`, `aether.mesh.rotate_vertices`, `aether.mesh.extrude_face`, `aether.mesh.delete_faces`, with `aether.mesh.describe` to introspect. Vertex and face IDs are monotonic + tombstoned for stability across sessions (per the mesh-editor-id-shape memory).

This was a useful spike — it validated the substrate render path, the hub MCP harness, and the agent-driving-substrate-via-mail loop. It produced a working bowl and (eventually) a wing-handled cup.

It also surfaced a clear failure mode: **the abstraction is wrong for the agent-driven authoring use case ADR-0026 commits to.** Specifically:

- The agent has to track vertex IDs across many operations to know what to manipulate next. A 100-vertex mesh has a 100-element ID space the agent must keep in working memory.
- The agent has to mentally simulate the mesh's geometry to predict what an op will produce. "Extrude face 47" has different meaning depending on the mesh's current state.
- Topology changes invalidate the agent's mental model. After an extrude, "vertex 50" exists where there was none before; after a delete, "face 47" is a tombstone the agent might still have in memory.
- Many edits later, the mesh is the result of a long history that exists nowhere as a single artifact. The asset is "the substrate's current state," not a file the agent (or a reviewer) can read.
- The teacup oracle test (PR-related discussion 2026-04-25) failed largely because the agent's mental model of the vertex graph couldn't track topology changes well enough to construct a recognizable teacup.

ADR-0026 made the architectural call that mesh content is authored as a primitive-composition DSL parsed at load time. The dsl-mesh spike (PRs 256, 257) then proved the inverse claim: an agent emitting **30 lines of DSL text** can produce a recognizable teapot in one shot, with no intermediate state to track. The DSL *is* the asset, the asset *is* the agent's working memory, and editing is rewriting text — exactly what LLMs are good at.

The two abstractions are not compatible. We can ship one or the other but not both as the load-bearing mesh editor.

## Decision

**The mesh editor is the DSL.** Agents author meshes by emitting DSL text and sending it to the editor; the editor parses, meshes, caches, and replays the result every tick. Iteration is rewriting the text and re-sending it; the editor hot-reloads on receipt.

Specifically:

- **Component crate `aether-mesh-editor-component` is rewritten** to be a DSL hot-loader. Crate name unchanged; internals replaced.
- **New mail vocabulary** (proposed details left to ADR-0053):
  - `aether.dsl_mesh.set_text { dsl }` — agent supplies inline DSL text. Component parses, meshes, replaces cache.
  - `aether.dsl_mesh.set_path { namespace, path }` — agent supplies a path; component fires `aether.io.read` to load the DSL from disk, then parses + meshes + replaces cache.
- **Old mail vocabulary is removed** from the component and from `aether-kinds`:
  - `aether.mesh.set_primitive`, `aether.mesh.translate_vertices`, `aether.mesh.scale_vertices`, `aether.mesh.rotate_vertices`, `aether.mesh.extrude_face`, `aether.mesh.delete_faces`, `aether.mesh.describe`, plus the response shape `aether.mesh.state`.
- **Hot reload semantics are by-replacement.** Each `set_text` or successful `set_path` reload drops the prior cache wholesale and installs the new triangle list. There is no diff/patch surface in v1.
- **The editor is stateless beyond the cache.** It does not retain DSL history, undo, or a "current edit position." History is the user's responsibility (git, file timestamps, agent transcripts).

### What "edit" means under this model

The agent's edit loop is:

1. Read the current DSL (from a file via `aether.io.read`, from agent memory, or from a fresh prompt).
2. Modify the text — change a profile point, add a `(translate ...)` wrapper, swap a torus for a sweep.
3. Send `aether.dsl_mesh.set_text` (or write the file and send `set_path`).
4. Capture a frame to verify.
5. Iterate or commit.

This is the workflow the spike validated for the teapot. The editor's job is to make step 3 → 4 fast (substrate hot-reloads in one tick).

### What this ADR does not do

- It does not specify the DSL grammar — that's ADR-0026 + ADR-0051.
- It does not specify the parser/mesher implementation crate layout — that's ADR-0053.
- It does not commit to a file format for `set_path`. The DSL text format defined by ADR-0026 IS the file format; no envelope.
- It does not commit to introspection mail (a `describe`-equivalent for the DSL). The agent already has the source text — there's nothing to introspect that the agent didn't just send. If a use case for "inspect the meshed result" appears, it lands as a separate ADR.

## Consequences

### Positive

- **The asset is the DSL text.** Git-versionable, diffable, reviewable, search-grep-able. No "the substrate's current state" as a hidden source of truth.
- **The agent's working memory matches what's on disk.** No vertex-ID tracking, no topology mental model. The agent edits text, exactly what it's good at.
- **The component is dramatically simpler.** No sparse vertex/face vectors, no tombstones, no monotonic ID allocator. Two handlers (`set_text`, `set_path`), one cache, one tick replayer.
- **One asset format across all mesh consumers.** The DSL editor and a future "load this scene at startup" path read the same files.
- **Iteration is fast and reversible.** Rewrite text, hot-reload, capture, repeat. Reverting is reverting the file.
- **Composition is free.** A scene with multiple meshes is one DSL document with one `(composition ...)` at the root, or several files loaded into separate component instances. Either works without new infrastructure.

### Negative

- **The retired vertex-editor API can no longer be used.** Any spike, demo, or external script that drove `aether.mesh.set_primitive` / `translate_vertices` / `extrude_face` / etc. via MCP must be rewritten to emit DSL instead. The mesh-editor-id-shape memory and related discussion artifacts are now historical context, not active guidance. Documented in the migration notes shipped with the implementing PR.
- **Local fine-grained edits cost a full re-mesh.** Tweaking one profile point re-meshes the whole DSL. For our teapot-class scale this is fast (sub-millisecond meshing for ~700 triangles), but a future complex scene might want diff-based reloads. Deferred until pressure shows up.
- **No incremental introspection.** The agent can't ask "what's the current vertex at id 47" because there is no id 47 — there's just text. If introspection becomes useful (e.g. "where is the bottom-tip of the handle in world coords?"), it needs a new query mail kind that walks the AST.
- **Existing memory entries about the mesh editor become partially stale.** Specifically `project_mesh_editor_spike_planned.md`, `project_mesh_editor_id_shape.md`, `project_teacup_oracle_test.md` all refer to the retired API. They remain useful as historical context but should not be read as active guidance after this ADR lands. Updates to those memories will reflect the supersession.

### Neutral

- **Multiple mesh editors per scene** are still supported — load the component multiple times under different names, drive each with different DSL. Works the same as the static-mesh viewer pattern.
- **The DSL parser/mesher lives in a library crate**, not the component, so other consumers (a DSL linter, a one-off CLI for OBJ export, future agents that want to mesh without going through the editor) can use it directly.

## Alternatives considered

- **Keep both abstractions side-by-side.** Rejected: ADR-0026's positioning is "the DSL is the only mesh authoring path." Shipping a parallel vertex-editor contradicts that and creates a "which path do I use?" question for every new component author. Two paths means neither is the path.
- **Keep the vertex editor as the editor; treat the DSL as an export format only.** Rejected: it inverts cause and effect. The DSL is the asset format per ADR-0026; the editor must therefore edit assets. An "edit a different format and export to assets" workflow is exactly the conventional-tool authoring pipeline ADR-0026 rejects.
- **Add a `compile_to_dsl` op to the vertex editor.** Rejected: vertex-graph → DSL inverse-mapping is hard (the DSL is structural; the vertex graph is geometric) and produces ugly machine-generated DSL that the agent can't easily edit further. The DSL is most useful when it's hand- or LLM-authored at the structural level.
- **Defer the editor rewrite until ADR-0053's promotion lands.** Rejected: the rewrite IS the implementation of ADR-0053's editor crate. They land together (or the editor lands second once the library is there).
- **Diff-based reload (send only the changed subtree).** Rejected for v1: full re-mesh is fast at our scale, and a diff protocol adds significant agent-side bookkeeping. Re-evaluate if scenes get large enough for full re-mesh to feel slow.

## Follow-up work

- ADR-0053 lands the promotion of the spike to a library crate plus the editor rewrite as one or two coordinated PRs.
- Update `project_mesh_editor_spike_planned.md`, `project_mesh_editor_id_shape.md`, `project_teacup_oracle_test.md` memories to reflect supersession after the editor PR lands.
- The `mcp__aether-hub__describe_component` MCP tool still surfaces the editor's input handlers. After the rewrite, it will show `set_text` and `set_path` instead of `set_primitive` / `translate_vertices` / etc. — agents that read the capabilities surface adapt automatically.
