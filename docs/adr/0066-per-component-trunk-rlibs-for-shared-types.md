# ADR-0066: Per-component trunk rlibs for shared types

- **Status:** Proposed
- **Date:** 2026-04-29

## Context

`aether-kinds` was conceived as the shared-rlib home for every wire-shape struct any component might want to send or receive. Today it holds two distinct populations under one roof:

- **Chassis primitives** — `Tick`, `Key`, `MouseMove`, `MouseButton`, `DrawTriangle`, `aether.audio.*`, `aether.io.*`, `aether.control.*`, `aether.observation.*`, `aether.camera` (the `view_proj` kind sent to the camera sink). The substrate itself emits or consumes these.
- **Component-pair contracts** — `aether.camera.create / .set_active / .set_mode / .orbit.set / .topdown.set / ...`, `aether.mesh.load`, formerly `aether.player.*`. Two cdylib components agree on a wire shape; neither involves the substrate.

Issue #394 surfaced the discomfort: `aether-kinds` becomes a coupling point any time a new component-pair contract appears, the boundary between "chassis primitive" and "component contract" is invisible at the source level, and the crate keeps growing with no natural cap.

The deeper constraint is **ownership**: third-party component authors physically cannot put types into `aether-kinds` because they don't own the crate. Rust does not provide finer-grained ownership than the crate boundary, so component-owned types must live in component-owned crates. Anything else only works for first-party contributors and breaks the moment external authors ship components.

This ADR finalizes how component-owned wire types are organized.

### Mechanism candidates considered (and dropped)

- **Submodule organization inside `aether-kinds`** — addresses the visual-boundary pain but cannot solve third-party ownership. Anyone without write access to `aether-kinds` is excluded from the model.
- **Dual `crate-type = ["cdylib", "rlib"]` on each component crate** with feature-gated `aether_component::export!()` — workable in principle, but cargo's feature unification across a single workspace build graph means `cargo build --workspace --target wasm32-unknown-unknown` either turns the FFI feature on for an rlib-as-dep view (linker collision on `#[no_mangle] receive_p32` etc.) or off for the cdylib build (no FFI exports). No combination of `CARGO_PRIMARY_PACKAGE`, `cfg(target_arch)`, or weak symbols rescues this. The linker constraint ("a cdylib emits `#[no_mangle]`, an rlib does not") is structural, not a flag to flip.
- **Wasm component model + WIT** — explicitly out of scope. Aether uses raw wasm modules with custom mail dispatch (per ADR-0006); switching is a much larger architectural shift than this ADR scopes.

## Decision

A "component" splits into two crates:

- **`aether-<thing>`** — rlib trunk. Public API for the component: kind types, parameter structs, math/geometry helpers that consumers might need, anything not bound to the wasm runtime. Never depends on `aether-component` (the SDK), never calls `export!()`, never emits `#[no_mangle]`.
- **`aether-<thing>-component`** — cdylib runtime. The deployable wasm. Depends on `aether-<thing>` for its own kind types, depends on `aether-component` for SDK + macros, calls `aether_component::export!(...)` in its own `lib.rs`. Never imported by another component.

A second component talking to `<thing>` depends on **`aether-<thing>`** only. It never imports `<thing>-component`; there is no rlib of `<thing>-component` available to other consumers.

Third parties get the identical shape: `coffeepots-thing` (rlib) + `coffeepots-thing-component` (cdylib). The convention is symmetric and not first-party-privileged.

`aether-kinds` shrinks to chassis primitives only — kinds the substrate itself emits or consumes:

- `aether.tick.Tick`, `aether.input.{Key, MouseMove, MouseButton}`
- `aether.draw_triangle` (render sink contract), `aether.camera` (camera sink contract — the `view_proj` kind)
- `aether.audio.*` (audio sink contracts)
- `aether.io.*` (io sink contracts)
- `aether.control.*` (control plane: load / drop / replace / window / subscribe and their results)
- `aether.observation.*` (frame_stats etc.)

Component-owned kinds currently squatting in `aether-kinds` migrate to their component's trunk crate as part of this ADR's rollout:

- `aether.camera.{create, destroy, set_active, set_mode, orbit.set, topdown.set, *_result}` and their parameter structs (`OrbitParams`, `TopdownParams`, `ModeInit`, etc.) → **`aether-camera`** (new).
- `aether.mesh.load` → **`aether-mesh-viewer`** (new), sibling to `aether-mesh-viewer-component`.

The retired `aether.player.*` kinds need no migration.

### Naming

| Crate | Role | Examples |
|---|---|---|
| `aether-<thing>` | rlib trunk: kinds + helpers + types; public API | `aether-camera`, `aether-mesh-viewer` |
| `aether-<thing>-component` | cdylib runtime: implementation, calls `export!()` | `aether-camera-component`, `aether-mesh-viewer-component` |

A README in each trunk crate ("runtime cdylib lives in `aether-<thing>-component`") covers the discoverability cost of the bare name.

The chassis-shared `aether-kinds` keeps its name; it is now scoped to substrate primitives only.

## Consequences

**Positive**

- Crate boundary aligns with ownership boundary. First-party and third-party authors get the same model with no special-casing.
- `aether-kinds` caps naturally at chassis primitives. New component-pair contracts add a sibling crate to the originating component, not a touch on the chassis-shared crate.
- Authoring intent is structurally legible: editing `aether-camera` means touching the camera contract; editing `aether-kinds` means touching a substrate primitive.
- No feature-flag gymnastics. No `default-features = false` chant for consumers. Workspace-wide `cargo build --target wasm32-unknown-unknown` works without xtask wrappers.
- Trunk crates can host non-runtime helpers later (CLI inspection tools, fixtures, math shorthands) without re-architecture.

**Negative**

- Crate count grows linearly with components. Rust workspaces handle this fine and rust-analyzer is unaffected, but `cargo metadata` + `Cargo.lock` get marginally larger.
- One-time migration cost: move `aether.camera.*` and `aether.mesh.load` (plus their param structs) into new trunk crates, update every consumer's imports, ship the trunk crates as new workspace members.
- Two crates per component means two `Cargo.toml` files per component to keep aligned (versions, edition, rust-version).
- Discoverability cost: someone glancing at `crates/aether-camera/` may miss `crates/aether-camera-component/`. Mitigated by the README pointer in the trunk.

**Neutral**

- Wire-format kind names (`aether.camera`, `aether.camera.create`, `aether.mesh.load`) are unchanged. The split is purely about source-code ownership; the schema-hashed kind ids on the wire (ADR-0030) round-trip identically.
- The `aether.camera*` kind-name namespace ends up split across two crates: `aether.camera` (the singular sink-contract kind, in `aether-kinds`) and `aether.camera.<verb>` (the component control surface, in `aether-camera`). This is acceptable — `aether.camera` and `aether.camera.create` are distinct strings dispatched to different mailboxes (`aether.sink.camera` vs the component's own mailbox). Future cleanup (e.g., renaming the sink contract kind to `aether.view_proj`) can address the visual ambiguity without disturbing this ADR.

**Follow-on work**

- New crate `aether-camera`; migrate `aether.camera.*` control kinds + `OrbitParams` / `TopdownParams` / `ModeInit` into it.
- New crate `aether-mesh-viewer`; migrate `aether.mesh.load` into it.
- Update `aether-camera-component` and `aether-mesh-viewer-component` to depend on their trunk crates.
- Update `CLAUDE.md`'s mesh + camera paragraphs and the chassis-sink-namespacing memory note.
- Document the convention in the workspace `README.md` so new components follow it from the start.

## Alternatives considered

- **Status quo (`aether-kinds` monolithic).** Rejected — cannot accommodate third-party-owned types; component-pair contracts force every author to touch the chassis-shared crate.
- **Submodule organization within `aether-kinds`.** Rejected — addresses visual boundaries but does not solve ownership; third-party authors still cannot push to `aether-kinds`.
- **Single `aether-protocol-kinds` rlib (halfway house: split chassis primitives from cross-component contracts into one extra rlib).** Rejected — replaces one dumping ground with two dumping grounds; doesn't align with ownership at all.
- **Dual `crate-type = ["cdylib", "rlib"]` on the component crate, feature-gated `export!()`.** Rejected — workspace-wide wasm builds break; consumer-side `default-features = false` is a footgun; the linker constraint ("a cdylib emits `#[no_mangle]`, an rlib does not") is structural, not a flag to flip.
- **Wasm component model + WIT.** Rejected as out of scope — switching off raw wasm modules to the component model is a much larger architectural change than this ADR scopes (would be an ADR of its own).

## References

- Issue 394 — original discomfort with `aether-kinds` as a dumping ground.
- ADR-0006 — wire + topology; raw wasm modules + custom dispatch.
- ADR-0028 — kinds custom section.
- ADR-0030 — name-derived kind ids; wire format unchanged by source-layout decisions.
- ADR-0033 — handler manifest.
