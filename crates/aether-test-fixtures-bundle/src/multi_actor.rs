//! ADR-0096 fixture: a multi-actor module. Two `WasmActor` types in one
//! crate, exported together via `export!(public = [RootManager, Panel])`.
//! Proves multi-type coexistence in a single wasm module (no duplicate-symbol
//! collision, which ADR-0014 §4 previously forbade), that the entry type (the
//! first export, `RootManager`) loads through an unmodified host, and that the
//! host can select the non-entry export (`Panel`) by its
//! `Addressable::NAMESPACE` (ADR-0096), at load or as an export-targeted
//! replace.
//!
//! Receive surfaces are deliberately distinct so a load or replace test can
//! prove which type was instantiated: both declare a `Ping` handler, but
//! `RootManager` is a strict receiver (no fallback) while `Panel` adds a
//! `#[fallback]`. The handlers do nothing; only their declared capability
//! groups are observed.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Ping;

/// Entry export — the first type in the `export!` list. An unmodified
/// host instantiates this one. Strict receiver: no `#[fallback]`.
pub struct RootManager;

#[actor(root)]
impl WasmActor for RootManager {
    const NAMESPACE: &'static str = "test.ui.root";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(RootManager)
    }

    /// Declares `Ping` so the entry type's capability group names it; the
    /// body is empty.
    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

/// Non-entry export — selectable at load or replace via
/// `export: "test.ui.panel"` (ADR-0096). Its `#[fallback]` distinguishes its
/// capability group from the entry type's strict receiver.
pub struct Panel;

#[actor(instanced, root)]
impl WasmActor for Panel {
    const NAMESPACE: &'static str = "test.ui.panel";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Panel)
    }

    /// Declares `Ping` so `Panel`'s capability group names it alongside the
    /// fallback; the body is empty.
    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}
