//! `WasmTrampoline` — the `NativeActor` every loaded wasm component runs
//! in. Each loaded wasm component is one trampoline instance (issue 634
//! Phase 4 PR 1), born under the guest's own published name: `NS`, `NS:key`,
//! or `parent/NS:key` (ADR-0241 §5, §6).
//!
//! ## Identity / runtime split (ADR-0122)
//!
//! The trampoline is split into an addressing **identity** and a
//! state-bearing **runtime**. [`WasmTrampoline`] is a ZST identity carrying
//! only the addressing surface — `Addressable` (`NAMESPACE` / `Resolver`),
//! the per-handler `HandlesKind<DropComponent>` / `HandlesKind<ReplaceComponent>`
//! / `HandlesKind<BootTeardown>` markers, and the `OnePer("component")` name-inventory entry — all emitted
//! always-on by `#[actor]`. The state-bearing runtime
//! (`WasmTrampolineState`, which owns the wasmtime `Component` plus the
//! `Engine` / `Linker` / `HubOutbound` handles, the resident `Module` and the
//! engine's module cache) and
//! its init config ([`WasmTrampolineConfig`], substrate/wasmtime-typed) live
//! behind the one `feature = "runtime"` gate (the `mod runtime` directory), so
//! a transport-only build of the identity never names the state nor pulls
//! `aether_substrate` through this cap.
//!
//! `spawn_guest::<WasmTrampoline, GuestControl>` resolves against the
//! identity — `spawn_guest` binds `H: Instanced + NativeActor`, which is the
//! identity — while the name is the guest's.
//!
//! ## Where this lives (issue 654)
//!
//! The trampoline sits next to [`crate::component::ComponentHostCapability`] —
//! its only consumer — and the namespace is whatever
//! `WasmTrampoline::NAMESPACE` says it is. Single declaration, cap-owned,
//! reachable on every target via the `Addressable` trait const, forward-fed
//! from [`EMBEDDED_SCOPE`] until #6869 retires it. No guest is named by it.
//!
//! ## Shape
//!
//! Instanced. Anything the trampoline doesn't handle
//! natively (today: `DropComponent`, `ReplaceComponent`, and the host's
//! `LoadDelivered` hand-off and `BootTeardown`) falls through the
//! `#[fallback]` (`forward_to_wasm`) to the wasm guest via `Component::deliver`.
//! The framework dispatcher reads from the trampoline's `NativeBinding`;
//! un-handled kinds reach `forward_to_wasm`; the guest's `send_mail_p32` /
//! `reply_mail_p32` host fns route through the same binding.
//!
//! ## Lifecycle
//!
//! - **Load**: `crate::component::ComponentHostCapability::on_load_component`
//!   stages a guest birth (`spawn_guest`) under the guest's published name
//!   and the agent-supplied key; the spawn path runs `init` which
//!   instantiates the wasm `Component` against the trampoline's binding.
//!   Once the birth completes the host hands its held reply to the
//!   trampoline as `LoadDelivered`, and the trampoline replies
//!   `LoadResult::Ok` to the requester in its own name, so the requester
//!   keeps the reply's stamped sender as its reference (ADR-0230 §3).
//! - **Drop**: `DropComponent` mail addressed to the trampoline's mailbox
//!   lands on `on_drop_component`, which drops the `Component` and clears the
//!   mailbox's accept-set. The trampoline (and its mailbox name) survives as an
//!   empty slot, refillable by `ReplaceComponent`.
//! - **Boot teardown** (ADR-0147): when a module's last non-boot actor
//!   unloads, the host sends `BootTeardown` through the module boot
//!   guest's control reference, and `on_boot_teardown` unloads the guest the way
//!   a drop does, with no reply.
//! - **Replace**: `ReplaceComponent` mail lands on `on_replace_component`,
//!   which checks the new bytes in through the engine's module cache
//!   (ADR-0241 §2), instantiates a new `Component` against the same binding
//!   and swaps `state.component`. ADR-0022 + ADR-0038 invariants hold
//!   because the inbox channel is the trampoline's `NativeBinding` and
//!   outlives the swap.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated dispatch owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{EMBEDDED_SCOPE, actor};

use crate::component::ComponentHostCapability;

// The runtime half — the whole `aether_substrate` / `wasmtime`-typed surface
// (imports, `WasmTrampolineState`, `WasmTrampolineConfig`, the replace
// helpers) — lives in the `runtime` directory, gated once here.
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
/// nor pulls `aether_substrate` through this cap. The component host stages
/// each guest through it — `spawn_guest::<WasmTrampoline, GuestControl>` —
/// under the guest's own name.
#[actor(instanced, child_of(ComponentHostCapability), child_of(WasmTrampoline))]
pub struct WasmTrampoline;
