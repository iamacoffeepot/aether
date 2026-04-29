# aether

A game engine being built collaboratively with Claude.

The vision is a harness where Claude sits as assistant, engineer, and designer — driving a running engine, observing it, modifying it. The architecture is a thin native **substrate** that owns I/O, GPU, and audio and hosts a WebAssembly runtime; engine **components** run as WASM modules and communicate with the substrate and with each other through a **mail** system.

## Reading the codebase

- Architectural decisions are recorded as ADRs under `docs/adr/`. Read in order to follow the design as it evolved; the latest ADRs describe current commitments.
- `CLAUDE.md` at the repo root is the working-directory guide for collaborating in this codebase.

## Component layout (ADR-0066)

A component is two crates:

- **`aether-<thing>`** — rlib trunk. Public API for the component: kind types (the mail shapes other components send to it), parameter structs, helpers. Never depends on `aether-component` (the SDK), never emits `#[no_mangle]`. This is what other components import.
- **`aether-<thing>-component`** — cdylib runtime. The deployable wasm. Depends on `aether-<thing>` for its own kind types, depends on `aether-component` for the SDK + macros, calls `aether_component::export!(...)` in `lib.rs`. Never imported by another component.

Examples in-tree: `aether-camera` + `aether-camera-component`, `aether-mesh-viewer` + `aether-mesh-viewer-component`. Third parties follow the same shape (`coffeepots-thing` + `coffeepots-thing-component`); the convention is symmetric and not first-party-privileged. `aether-kinds` is reserved for chassis primitives the substrate itself emits or consumes.
