# ADR-0053: Promote `dsl-mesh-spike` to `aether-dsl-mesh` library crate

- **Status:** Proposed
- **Date:** 2026-04-26
- **Implements:** ADR-0026, ADR-0051, ADR-0052

## Context

ADR-0026 committed the engine to a Lisp-syntactic primitive-composition DSL as the only mesh authoring path. ADR-0051 pinned the v1 vocabulary syntax and promoted torus + sweep from parked v2 into v1. ADR-0052 retired the vertex/face stateful mesh editor in favour of a DSL hot-loader that lives in the existing `aether-mesh-editor-component` crate.

The implementation of all three ADRs currently lives in `spikes/dsl-mesh-spike/` — its own standalone Cargo workspace per ADR-0003's spike convention. The spike has shipped:

- A typed AST (`src/ast.rs`) covering the full v1 vocabulary plus structural operators.
- A parser (`src/parse.rs`) and serializer (`src/serialize.rs`) over `lexpr` s-expressions, with round-trip tests.
- A mesher (`src/mesh.rs`) that handles `box`, `lathe`, `torus`, `sweep` (with `:scales` and parallel-transport framing), `composition`, `translate`, `rotate`, `scale`. The remaining v1 nodes (`cylinder`, `cone`, `wedge`, `sphere`, `extrude`, `mirror`, `array`) return `MeshError::NotYetImplemented`.
- An OBJ exporter (`src/obj.rs`) used by the spike's CLI binary (`src/main.rs`) and consumed by the `aether-static-mesh-component` viewer for end-to-end validation through the substrate render path.
- The `examples/teapot.dsl` model that ADR-0052's case study cites.

The spike has paid off: the abstraction is right, the wire surface is right, and the teapot validates the end-to-end loop. To use it from the rewritten mesh editor (per ADR-0052), it must live somewhere the workspace can `path = "..."` into. Spikes are structurally ineligible for that — they're standalone workspaces by ADR-0003 to keep their dependency graph isolated from the engine's.

This ADR pins the promotion plan so it doesn't accidentally drag in shape decisions that don't belong (a CLI binary the engine ships, a render dependency, a host-fn surface). The spike's CLI is useful for development but is not engine code.

## Decision

Promote `spikes/dsl-mesh-spike/` to `crates/aether-dsl-mesh/` as a **library-only** workspace member. The component rewrite per ADR-0052 lands as a separate change that depends on this crate.

### Crate scope

`aether-dsl-mesh` exposes three layers, each from a separate module:

- `ast` — the typed `Node` enum and helper types (`Axis`, etc.).
- `parse` — `parse(text: &str) -> Result<Node, ParseError>`. s-expression → typed AST.
- `serialize` — `serialize(node: &Node) -> String`. Typed AST → s-expression text. Round-trip with `parse`.
- `mesh` — `mesh(node: &Node) -> Result<Vec<Triangle>, MeshError>`. Typed AST → flat triangle list. The exposed `Triangle` is `{ vertices: [[f32; 3]; 3], color: u32 }` — same shape the spike already ships.
- `obj` — `to_obj(triangles: &[Triangle]) -> String`. Optional convenience for tooling/tests; doesn't depend on the substrate render path.

### What the crate does NOT contain

- **No CLI binary.** The spike's `src/main.rs` (DSL-text → OBJ-on-stdout) is useful for one-off development and stays available as `examples/dsl_to_obj.rs` so `cargo run --example dsl_to_obj -- examples/teapot.dsl` still works. It is not an engine artefact.
- **No rendering.** The crate produces triangles. Whoever consumes them owns the render path. The mesh editor component ships triangles as `aether.draw_triangle` mail per the existing pattern.
- **No host-fn surface, no MCP tool surface, no mail kinds.** Those live in `aether-kinds` and the component crates. This crate is a pure library.
- **No async, no I/O.** Reading DSL from a file is the component's job (via the `aether.io.read` sink per ADR-0041). Parsing and meshing are CPU-only synchronous calls.
- **No incremental re-meshing API.** ADR-0052 commits to by-replacement hot reload in v1. The crate exposes `mesh(node)` only; no `mesh_diff(old, new)`.

### Dependencies

Carries forward the spike's two: `lexpr` and `thiserror`. Both already vetted against the engine's dep policy (small, pure-Rust, no transitive surprises). Dev-dependency `pretty_assertions` likewise carries forward.

### Layout

```
crates/aether-dsl-mesh/
  Cargo.toml          # workspace member, library only
  src/
    lib.rs            # re-exports ast, parse, serialize, mesh, obj
    ast.rs
    parse.rs
    serialize.rs
    mesh.rs
    obj.rs
  examples/
    dsl_to_obj.rs     # the spike's CLI moved here verbatim
    teapot.dsl        # moved from spike examples/, used by tests
  tests/
    # spike's existing tests/ moved here verbatim
```

The spike directory `spikes/dsl-mesh-spike/` is deleted in the same PR so there is no parallel-source confusion. The teapot DSL becomes the crate's authoritative example.

### Cargo.toml shape

```toml
[package]
name = "aether-dsl-mesh"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
lexpr = "0.2"
thiserror = "2"

[dev-dependencies]
pretty_assertions = "1"
```

Workspace root `Cargo.toml` adds `crates/aether-dsl-mesh` to its `members` list. No new workspace dependencies; the two used here are crate-local.

### Sequencing relative to ADR-0051's leftover meshers

ADR-0051's "promoted to v1" claim is currently aspirational for `cylinder`, `cone`, `wedge`, `sphere`, `extrude`, `mirror`, `array` — the spike returns `NotYetImplemented` for all of them. This ADR's promotion happens **before** those meshers are filled in, for two reasons:

1. The crate-shape decision is independent of which mesher functions are populated. Promoting now means the editor rewrite per ADR-0052 can begin in parallel with the remaining meshers.
2. Filling in meshers inside the spike produces a larger move PR, with diff noise that obscures the promotion itself. Two small PRs (promote, then complete v1) review more cleanly than one large one.

The remaining meshers land as a follow-up PR against the new crate, not against the spike.

### Sequencing relative to the editor rewrite (ADR-0052)

The editor rewrite lands as a **separate** PR after this promotion. It needs `aether-dsl-mesh` as a path dependency, so the promotion must merge first. The two PRs are coordinated but distinct because:

- The promotion is a mechanical move + workspace wire-up. It should review fast.
- The editor rewrite removes mail kinds (`aether.mesh.set_primitive`, `translate_vertices`, etc.), adds new kinds (`aether.dsl_mesh.set_text`, `aether.dsl_mesh.set_path`), and restructures the component internals. It needs more careful review.

Bundling them risks the editor rewrite's review burden gating the promotion.

## Consequences

### Positive

- **The mesh editor (per ADR-0052) becomes implementable.** A workspace `path = "..."` dependency on `aether-dsl-mesh` is the only thing standing between today's spike and a real component.
- **The DSL is reusable beyond the editor.** A future scene loader, asset linter, or one-off OBJ-export tool depends on `aether-dsl-mesh` directly without going through a component or mail. The spike's "is this the right abstraction?" question is settled in favour of "yes, ship it as a library."
- **Spike directory disappears.** No long-lived parallel source tree where someone might fix a bug in one place but not the other. The crate is the source of truth.
- **Crate boundary forces a clean library API.** The spike's CLI was free to dip into `mesh::*` internals; the library has to expose its surface deliberately. The decision to keep the API to `parse` / `serialize` / `mesh` / `to_obj` is the contract.

### Negative

- **The spike's CI artefact (the standalone `Cargo.lock`) is gone.** That `Cargo.lock` was incidentally useful for catching dep-graph regressions in isolation; rolled into the workspace lock, regressions in `lexpr` or `thiserror` would surface workspace-wide rather than spike-local. The cost is small — both deps are stable — but worth naming.
- **Workspace member count goes up by one.** Build times for `cargo build` (no `-p`) get marginally longer. Negligible in practice; mentioned for completeness.
- **Spike-as-experiment status is consumed.** Promotion is the spike claiming the engine's stability bar. Future breaking changes to the AST or mesher API are now PR-reviewed engine changes, not spike iteration.

### Neutral

- **The teapot DSL moves with the crate.** It's the canonical example regardless of where it lives; the move is a path update for the OBJ-export workflow, nothing else.
- **`aether-static-mesh-component` is unaffected.** It loads OBJ from the io sink, not DSL. It will continue to consume the OBJ output of `cargo run --example dsl_to_obj` exactly as it does today.

## Alternatives considered

- **Promote and finish v1 meshers in one PR.** Rejected: bundles a mechanical move with semantic implementation work. Splits cleanly; the move PR reviews on "is the new crate shape right?", the meshers PR reviews on "is each new mesher correct?". Different review questions, different PRs.
- **Promote and rewrite the editor in one PR.** Rejected: see sequencing above. The editor rewrite is the larger semantic change and shouldn't gate the promotion.
- **Keep the spike, copy needed modules into the editor crate.** Rejected: produces two diverging copies of the parser/AST/mesher. ADR-0026 and ADR-0052 both depend on the DSL being one canonical thing across the engine. A library crate is the correct factoring; a vendored copy is not.
- **Split parser/AST/serialize into one crate and mesher into another.** Rejected for v1: the mesher depends on the AST and nothing else, and the AST is no use without the parser. Splitting now is premature factoring against a problem that doesn't exist (no consumer wants AST without mesher or mesher without AST). Revisit if a consumer wants only the parser (e.g. a syntax-highlighting LSP) and the dep weight bothers them.
- **Move the CLI into a separate `aether-dsl-mesh-cli` crate.** Rejected for v1: it's one short file (`dsl_to_obj.rs`) used during development. `cargo run --example dsl_to_obj` is the same ergonomics with one fewer crate.
- **Delay promotion until v1 meshers are complete.** Rejected: the editor rewrite per ADR-0052 needs the crate to exist as a path-importable library. Blocking that on filling in mesher stubs would idle the editor rewrite for no benefit; the partial mesher set already meshes the teapot.

## Follow-up work

- This ADR's PR cluster includes ADR-0051 and ADR-0052. After the cluster lands, the implementation cascade is:
  1. Promote `spikes/dsl-mesh-spike/` → `crates/aether-dsl-mesh/` per this ADR. (Mechanical move PR.)
  2. Finish v1 meshers (`cylinder`, `cone`, `wedge`, `sphere`, `extrude`, `mirror`, `array`) inside the new crate. (Implementation PR.)
  3. Rewrite `aether-mesh-editor-component` per ADR-0052 to depend on `aether-dsl-mesh` and expose `aether.dsl_mesh.set_text` / `set_path`. (Editor PR.)
  4. Wire-in PR: update demos, examples, MCP harness section in `CLAUDE.md`, and stale memory entries.
- The retired vertex-editor mail kinds (`aether.mesh.set_primitive`, `translate_vertices`, `scale_vertices`, `rotate_vertices`, `extrude_face`, `delete_faces`, `describe`, `state`) are deleted from `aether-kinds` in step 3. Anything in-tree that referenced them is updated in step 4.
