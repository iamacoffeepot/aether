//! ADR-0096 / ADR-0097 fixture: a multi-actor module. Two `WasmActor`
//! types in one crate, exported together via `export!(RootManager,
//! Panel)`. Proves multi-type coexistence in a single wasm module (no
//! duplicate-symbol collision, which ADR-0014 §4 previously forbade),
//! that the entry type (the first export, `RootManager`) loads through
//! an unmodified host, that the host can select the non-entry export
//! (`Panel`) by its `Addressable::NAMESPACE` (ADR-0096), and that `RootManager`
//! can spawn a `Panel` sibling at runtime via `ctx.spawn_child::<Panel>`
//! (ADR-0097).
//!
//! Receive surfaces are deliberately distinct so a load test can prove
//! which type was instantiated: `RootManager` is a strict receiver (one
//! `Ping` handler, no fallback); `Panel` adds a `#[fallback]`. On `Ping`,
//! `RootManager` spawns `Ping.seq.max(1)` `Panel` siblings — from a
//! single `receive` when `seq > 1`, covering issue iamacoffeepot/aether#2503's
//! multi-spawn-per-receive path — and each spawned `Panel` broadcasts a
//! `TickObserved` to the substrate-harness observer, so a scenario can confirm
//! every spawned sibling is addressable and live.

// `#[handler]` / `#[fallback]` methods take `&mut self` to match the
// dispatch ABI even when stateless.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, Mail, MailSender, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Ping;
use aether_test_fixtures_kinds::{SUBSTRATE_BENCH_OBSERVER_MAILBOX_NAME, TickObserved};

/// Entry export — the first type in the `export!` list. An unmodified
/// host instantiates this one. Strict receiver: no `#[fallback]`.
pub struct RootManager;

#[actor]
impl WasmActor for RootManager {
    const NAMESPACE: &'static str = "test.ui.root";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(RootManager)
    }

    /// ADR-0097: on `Ping`, spawn `seq.max(1)` `Panel` siblings from the
    /// same resident module — `seq > 1` drives multiple `spawn_child`
    /// calls within this one `receive`, which is exactly the shape
    /// issue #2503 covers (a second staged sibling spawn must not be
    /// dropped). `Subname::Counter` gives each spawn a bare counter
    /// discriminator (`0`, `1`, …); the returned `MailboxId`s are
    /// fire-and-forget here.
    #[handler::single]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_>, ping: Ping) {
        for _ in 0..ping.seq.max(1) {
            let _ = ctx.spawn_child::<Panel>(Subname::Counter, &());
        }
    }
}

/// Sibling export — selectable at load via `export: "test.ui.panel"`
/// (ADR-0096) and spawnable at runtime by `RootManager` (ADR-0097).
/// `Instanced` so it satisfies the `spawn_child` bound. Carries a
/// `#[fallback]` so its capability group is observably distinct from the
/// entry type's strict receiver.
pub struct Panel;

#[actor(instanced)]
impl WasmActor for Panel {
    const NAMESPACE: &'static str = "test.ui.panel";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Panel)
    }

    /// On `Ping`, broadcast a `TickObserved` to the substrate-harness observer
    /// so a scenario can confirm a spawned `Panel` is addressable and
    /// dispatches mail.
    #[handler::single]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_>, _ping: Ping) {
        ctx.send_to_named::<TickObserved>(SUBSTRATE_BENCH_OBSERVER_MAILBOX_NAME, &TickObserved { count: 1 });
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}
