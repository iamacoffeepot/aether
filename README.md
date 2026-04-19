# aether

A game engine being built collaboratively with Claude.

The vision is a harness where Claude sits as assistant, engineer, and designer — driving a running engine, observing it, modifying it. The architecture is a thin native **substrate** that owns I/O, GPU, and audio and hosts a WebAssembly runtime; engine **components** run as WASM modules and communicate with the substrate and with each other through a **mail** system.

## Reading the codebase

- Architectural decisions are recorded as ADRs under `docs/adr/`. Read in order to follow the design as it evolved; the latest ADRs describe current commitments.
- `CLAUDE.md` at the repo root is the working-directory guide for collaborating in this codebase.
