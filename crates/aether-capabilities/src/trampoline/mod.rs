//! `WasmTrampoline` — the `NativeActor` every loaded wasm component runs
//! as. Each loaded wasm component is one trampoline instance addressed at
//! `aether.embedded:NAME` (issue 634 Phase 4 PR 1).
//!
//! ## Identity / runtime split (ADR-0122)
//!
//! The trampoline is split into an addressing **identity** and a
//! state-bearing **runtime**. [`WasmTrampoline`] is a ZST identity carrying
//! only the addressing surface — `Addressable` (`NAMESPACE` / `Resolver`),
//! the per-handler `HandlesKind<DropComponent>` / `HandlesKind<ReplaceComponent>`
//! markers, and the `OnePer("component")` name-inventory entry — all emitted
//! always-on by `#[actor]`. The state-bearing runtime
//! (`WasmTrampolineState`, which owns the wasmtime `Component` plus the
//! `Engine` / `Linker` / `Registry` / `Mailer` / `HubOutbound` handles) and
//! its init config ([`WasmTrampolineConfig`], substrate/wasmtime-typed) live
//! behind the one `feature = "runtime"` gate (the `mod runtime` directory), so
//! a transport-only build of the identity never names the state nor pulls
//! `aether_substrate` through this cap.
//!
//! `WasmTrampoline::NAMESPACE`, `spawn_child::<WasmTrampoline>`, and
//! `resolve_actor::<WasmTrampoline>` resolve against the identity — `spawn_child`
//! / `resolve_actor` bind `A: Instanced + NativeActor`, which is the identity.
//!
//! ## Where this lives (issue 654)
//!
//! The trampoline sits next to [`crate::component::ComponentHostCapability`] —
//! its only consumer — and the namespace is whatever
//! `WasmTrampoline::NAMESPACE` says it is. Single declaration, cap-owned,
//! reachable on every target via the `Addressable` trait const. ADR-0097: the
//! substrate's `TRAMPOLINE_NAMESPACE` forward-feeds the same
//! [`EMBEDDED_SCOPE`] const, collapsing the
//! former two-literal mirror into one source; the
//! `trampoline_namespace_matches_substrate` test guards the match.
//!
//! ## Shape
//!
//! Instanced. Anything the trampoline doesn't handle
//! natively (today: `DropComponent`, `ReplaceComponent`) falls through the
//! `#[fallback]` (`forward_to_wasm`) to the wasm guest via `Component::deliver`.
//! The framework dispatcher reads from the trampoline's `NativeBinding`;
//! un-handled kinds reach `forward_to_wasm`; the guest's `send_mail_p32` /
//! `reply_mail_p32` host fns route through the same binding.
//!
//! ## Lifecycle
//!
//! - **Load**: `crate::component::ComponentHostCapability::on_load_component`
//!   spawns a trampoline via the runtime spawn machinery (subname = the
//!   agent-supplied component name); the spawn path runs `init` which
//!   instantiates the wasm `Component` against the trampoline's binding.
//! - **Drop**: `DropComponent` mail addressed to the trampoline's mailbox
//!   lands on `on_drop_component`, which drops the `Component` and clears the
//!   mailbox's accept-set. The trampoline (and its mailbox name) survives as an
//!   empty slot, refillable by `ReplaceComponent`.
//! - **Replace**: `ReplaceComponent` mail lands on `on_replace_component`,
//!   which instantiates a new `Component` against the same binding and swaps
//!   `state.component`. ADR-0022 + ADR-0038 invariants hold because the inbox
//!   channel is the trampoline's `NativeBinding` and outlives the swap.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated dispatch owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root so the
// `#[actor]`-emitted `impl HandlesKind<K> for WasmTrampoline {}` markers
// (always-on) resolve. The per-handler `HandlerEntry` inventory the same
// `#[actor]` emits (native-only, `not(wasm)`) names each handler's reply kind,
// so `DropResult` / `ReplaceResult` are imported under the matching gate.
use aether_actor::{EMBEDDED_SCOPE, actor};
use aether_kinds::{DropComponent, ReplaceComponent};
#[cfg(not(target_family = "wasm"))]
use aether_kinds::{DropResult, ReplaceResult};

// The runtime half — the whole `aether_substrate` / `wasmtime`-typed surface
// (imports, `WasmTrampolineState`, `WasmTrampolineConfig`, the replace /
// sibling-spawn helpers) — lives in the `runtime` directory, gated once here.
// The `#[runtime] impl` sits beside its state there.
#[cfg(feature = "runtime")]
mod runtime;

// The init config is substrate/wasmtime-typed (runtime-half), so its
// re-export re-gates to `feature = "runtime"`.
#[cfg(feature = "runtime")]
pub use runtime::WasmTrampolineConfig;

/// The wasm-trampoline **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the `OnePer("component")`
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`WasmTrampolineState`, which holds the wasmtime
/// `Component` and the substrate handles) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names the state
/// nor pulls `aether_substrate` through this cap. External consumers address
/// this name — `spawn_child::<WasmTrampoline>`, `resolve_actor::<WasmTrampoline>`,
/// `WasmTrampoline::NAMESPACE`.
#[actor(instanced)]
pub struct WasmTrampoline;

#[cfg(test)]
mod tests {
    use aether_actor::{Addressable, EMBEDDED_SCOPE};
    #[cfg(feature = "runtime")]
    use aether_substrate::actor::wasm::component::TRAMPOLINE_NAMESPACE;

    /// ADR-0099 §5/§6, ADR-0119: `WasmTrampoline::NAMESPACE`
    /// (capabilities) forward-feeds [`EMBEDDED_SCOPE`] — `aether-actor`'s sole
    /// owner of the `"aether.embedded"` literal, which sits below this crate.
    /// This guards that the identity ZST keeps resolving to the scope
    /// namespace, so an embedded component registers under and resolves to it.
    #[test]
    fn trampoline_namespace_matches_substrate() {
        assert_eq!(
            <super::WasmTrampoline as Addressable>::NAMESPACE,
            EMBEDDED_SCOPE,
        );
        assert_eq!(EMBEDDED_SCOPE, "aether.embedded");
        // The substrate's `TRAMPOLINE_NAMESPACE` forward-feeds the same const;
        // only reachable when the substrate-typed runtime half is compiled.
        #[cfg(feature = "runtime")]
        assert_eq!(TRAMPOLINE_NAMESPACE, EMBEDDED_SCOPE);
    }
}
