//! `aether.component` wasm-component-lifecycle cap (ADR-0022, ADR-0038,
//! ADR-0122).
//!
//! Two modules that are one capability. [`component`] is the
//! `aether.component` mailbox itself — the [`ComponentHostCapability`]
//! singleton that receives `aether.component.{load,drop,replace}` mail —
//! and [`trampoline`] is the [`WasmTrampoline`] `NativeActor` that every
//! loaded wasm component actually runs as, one instance per component,
//! addressed at `aether.embedded:NAME`.
//!
//! ## Load path
//!
//! `LoadComponent` mail reaches the cap, which spawns a trampoline child
//! and instantiates the guest wasm `Component` against that trampoline's
//! binding. `DropComponent` and `ReplaceComponent` flow through the cap
//! as well: it forwards each to the addressed trampoline preserving the
//! original `reply_to`, so the trampoline replies straight back to the
//! agent. The cap keeps no per-component bookkeeping — the trampoline
//! manages its own lifecycle, dispatch rides the framework's
//! `NativeActor` loop, and an ADR-0022 in-place replace is a `Component`
//! swap inside the trampoline behind a stable mailbox handle.
//!
//! ## Crate shape
//!
//! Extracted by the arc that dissolved the capabilities monolith
//! (iamacoffeepot/aether#3756) as a per-cap crate.
//! The cap and its trampoline move as one unit because they are one
//! capability split across two modules: `component` names
//! `WasmTrampoline` / `WasmTrampolineConfig` for real — it spawns the
//! trampoline, reads its `NAMESPACE` to build peer addresses, and hands
//! it its init config — while the trampoline's references back to the
//! cap are rustdoc links only. Neither half is useful alone, and
//! splitting them would put a hard dependency edge between two crates
//! that share one mailbox family.
//!
//! `aether-http` depends on this crate because its deferred-reply path
//! names [`ComponentHostCapability`] to resolve the handler component it
//! answers through. That is a genuine downward use, not a facade:
//! neither crate re-exports the other's names, so a consumer of the
//! component cap depends on this crate directly.
//!
//! The ADR-0122 identity/runtime split rides the `runtime` feature: the
//! ZST cap and trampoline identities with their `Addressable`,
//! per-handler `HandlesKind`, and name-inventory markers compile
//! always-on, so a transport-only wasm guest can address
//! `ctx.actor::<ComponentHostCapability>()` and resolve a loaded peer
//! without naming the substrate. The state-bearing wasmtime half
//! (`ComponentHostCapabilityState`, `WasmTrampolineState`, and the
//! [`ComponentHostParams`] / [`WasmTrampolineConfig`] init bundles that
//! hold `Arc<Engine>` / `Arc<Linker<ComponentCtx>>`) is gated behind it,
//! so nothing but a native chassis pulls wasmtime through this crate.

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
// ADR-0170: the params provider registry a chassis seeds and extends, plus the
// load-time context its providers read. Same gate as `ComponentHostParams` —
// the registry rides it into the cap.
#[cfg(feature = "runtime")]
pub use component::{DuplicateParamProvider, LoadContext, MissingParamProvider, ParamProvider, ParamProviderRegistry};
pub use trampoline::WasmTrampoline;
#[cfg(feature = "runtime")]
pub use trampoline::WasmTrampolineConfig;
