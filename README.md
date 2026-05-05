# aether

A game engine being built collaboratively with Claude.

The vision is a harness where Claude sits as assistant, engineer, and designer — driving a running engine, observing it, modifying it. The architecture is a thin native **substrate** that owns I/O, GPU, and audio and hosts a WebAssembly runtime; engine **components** run as WASM modules and communicate with the substrate and with each other through a **mail** system.

## Reading the codebase

- Architectural decisions are recorded as ADRs under `docs/adr/`. Read in order to follow the design as it evolved; the latest ADRs describe current commitments.
- `CLAUDE.md` at the repo root is the working-directory guide for collaborating in this codebase.

## Component layout (ADR-0066, amended by issue 552 stage 1.5)

A component lives in one dual-output crate (`crate-type = ["cdylib", "rlib"]`):

- The crate's **rlib** is the public API for *talking to* the component — kind types (the mail shapes other components send to it), parameter structs, helpers. Other components import it for the wire shapes.
- The crate's **cdylib** is the deployable wasm. The runtime (`#[actor] impl WasmActor for …`) sits next to the trunk types in the same crate; the wasm `export!()` invocation produces the FFI exports the substrate links against. The host-target rlib build leaves those exports inert so integration tests can link the same artifact.

Examples in-tree: `aether-camera`, `aether-mesh-viewer`. Third parties follow the same shape (`coffeepots-thing`); the convention is symmetric and not first-party-privileged. `aether-kinds` is reserved for chassis primitives the substrate itself emits or consumes. ADR-0066's prior `<thing>` + `<thing>-component` two-crate split was consolidated by issue 552 stage 1.5 — the cdylib + rlib pair lives in one crate now.
