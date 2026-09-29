//! `aether.component` capability: publishing, spawning, loading, dropping,
//! and replacing wasm components.
//!
//! Two modules, one capability. [`component`] is the `aether.component`
//! mailbox itself, the [`ComponentHostCapability`] singleton that receives
//! `aether.component.{publish,spawn,load,drop,replace}`. [`trampoline`] is the
//! [`WasmTrampoline`] native actor that every loaded wasm component runs in,
//! one instance per component, named by the guest's own published namespace:
//! `NS`, `NS:key`, or `parent/NS:key` (ADR-0241 §5).
//!
//! `Publish` binds a module's namespaces to it (ADR-0241 §3, §9), and
//! `Spawn` asks for an instance of a published type: the capability spawns a
//! trampoline under the guest's name and instantiates the guest wasm
//! `Component` against that trampoline's binding, or answers with the
//! instance already live there. `LoadComponent` is a publish then a spawn.
//! `DropComponent` is handed to the addressed trampoline with the original
//! caller as its reply target, so the trampoline answers the caller
//! directly. The trampoline manages its own lifecycle and dispatch rides the
//! framework's `NativeActor` loop. A drop closes the trampoline, so its name
//! tombstones and is never loaded again (ADR-0241 §8). `ReplaceComponent`
//! republishes a module as one group (ADR-0241 §7): the host drives every
//! live instance of the module's namespaces through a prepare, commit or
//! abort, and each trampoline swaps its `Component` behind a stable mailbox
//! handle, so a mailbox id or route cache taken before the swap stays valid
//! (ADR-0022); a `Publish` or a load of a successor module republishes the
//! same way. [`kinds`] holds the
//! capability's own internal mail, such as the contexts its staged loads
//! carry.
//!
//! The `runtime` feature carries the wasmtime half:
//! `ComponentHostCapabilityState`, `WasmTrampolineState`, and the
//! [`ComponentHostParams`] / [`WasmTrampolineConfig`] init bundles holding
//! `Arc<Engine>` / `Arc<Linker<ComponentCtx>>`. The host owns the engine's
//! one module cache and hands each trampoline its checked-in `Module`, so a
//! module is compiled and its sections parsed once per content hash
//! (ADR-0241 §2). The identities and their addressing markers compile
//! always-on, so a transport-only wasm guest can mail `ctx.send::<ComponentHostCapability>(..)` and resolve a loaded peer
//! without naming the substrate (ADR-0122).

#![forbid(unsafe_code)]

extern crate alloc;

pub mod component;
pub mod kinds;
pub mod trampoline;

pub use component::ComponentHostCapability;
// `ComponentHostParams` is wasmtime-bound (it holds `Arc<Engine>` /
// `Arc<Linker<ComponentCtx>>`). Under the ADR-0122 split it lives behind
// the `feature = "runtime"` gate (only the runtime half names it), so it
// re-exports only when that feature is on — a transport-only build sees the
// cap stub via `ComponentHostCapability` for `ctx.send::<ComponentHostCapability>(..)`
// addressing without dragging the wasmtime stack in.
#[cfg(feature = "runtime")]
pub use component::ComponentHostParams;
pub use trampoline::WasmTrampoline;
#[cfg(feature = "runtime")]
pub use trampoline::WasmTrampolineConfig;
