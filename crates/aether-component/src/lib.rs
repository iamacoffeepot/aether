//! `aether.component` capability: loading, dropping, and replacing wasm
//! components.
//!
//! Two modules, one capability. [`component`] is the `aether.component`
//! mailbox itself, the [`ComponentHostCapability`] singleton that receives
//! `aether.component.{load,drop,replace}`. [`trampoline`] is the
//! [`WasmTrampoline`] native actor that every loaded wasm component runs as,
//! one instance per component, addressed at `aether.embedded:NAME`.
//!
//! `LoadComponent` reaches the capability, which spawns a trampoline child and
//! instantiates the guest wasm `Component` against that trampoline's binding.
//! `DropComponent` and `ReplaceComponent` are forwarded to the addressed
//! trampoline with the original `reply_to` intact, so the trampoline answers
//! the caller directly. The capability keeps no per-component bookkeeping: the
//! trampoline manages its own lifecycle, dispatch rides the framework's
//! `NativeActor` loop, and an in-place replace swaps the `Component` inside
//! the trampoline behind a stable mailbox handle, so a mailbox id or route
//! cache taken before the swap stays valid (ADR-0022).
//!
//! The `runtime` feature carries the wasmtime half:
//! `ComponentHostCapabilityState`, `WasmTrampolineState`, and the
//! [`ComponentHostParams`] / [`WasmTrampolineConfig`] init bundles holding
//! `Arc<Engine>` / `Arc<Linker<ComponentCtx>>`. The identities and their
//! addressing markers compile always-on, so a transport-only wasm guest can
//! address `ctx.actor::<ComponentHostCapability>()` and resolve a loaded peer
//! without naming the substrate (ADR-0122).

#![forbid(unsafe_code)]

extern crate alloc;

pub mod component;
pub mod trampoline;

pub use component::{ComponentHostCapability, resolve_embedded};
// `ComponentHostParams` is wasmtime-bound (it holds `Arc<Engine>` /
// `Arc<Linker<ComponentCtx>>`). Under the ADR-0122 split it lives behind
// the `feature = "runtime"` gate (only the runtime half names it), so it
// re-exports only when that feature is on — a transport-only build sees the
// cap stub via `ComponentHostCapability` for typed `ctx.actor::<...>()`
// addressing without dragging the wasmtime stack in.
#[cfg(feature = "runtime")]
pub use component::ComponentHostParams;
pub use trampoline::WasmTrampoline;
#[cfg(feature = "runtime")]
pub use trampoline::WasmTrampolineConfig;
