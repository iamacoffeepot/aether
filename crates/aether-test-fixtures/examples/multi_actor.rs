//! ADR-0096 fixture: a multi-actor module. Two `FfiActor` types in one
//! crate, exported together via `export!(RootManager, Panel)`. Proves
//! multi-type coexistence in a single wasm module (no duplicate-symbol
//! collision, which ADR-0014 §4 previously forbade), that the entry
//! type (the first export, `RootManager`) loads through an unmodified
//! host, and that the host can select the non-entry export (`Panel`)
//! by its `Actor::NAMESPACE`.
//!
//! The two types carry deliberately distinct receive surfaces so a load
//! test can prove which one was instantiated: `RootManager` is a strict
//! receiver (one `Ping` handler, no fallback) while `Panel` adds a
//! `#[fallback]`. The resolved `aether.kinds.inputs` group for a
//! selected export must match the type that load picked.

// `#[handler]` / `#[fallback]` methods take `&mut self` to match the
// dispatch ABI even when stateless.
#![allow(clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, Mail, Resolver, actor};
use aether_kinds::Ping;

/// Entry export — the first type in the `export!` list. An unmodified
/// host instantiates this one. Strict receiver: no `#[fallback]`.
pub struct RootManager;

#[actor]
impl FfiActor for RootManager {
    const NAMESPACE: &'static str = "ui.root";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(RootManager)
    }

    #[handler]
    fn on_ping(&mut self, _ctx: &mut FfiCtx<'_>, _ping: Ping) {}
}

/// Sibling export — selected by passing `export: "ui.panel"` to the
/// load. Carries a `#[fallback]` so its capability group is observably
/// distinct from the entry type's strict receiver.
pub struct Panel;

#[actor]
impl FfiActor for Panel {
    const NAMESPACE: &'static str = "ui.panel";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(Panel)
    }

    #[handler]
    fn on_ping(&mut self, _ctx: &mut FfiCtx<'_>, _ping: Ping) {}

    #[fallback]
    fn on_other(&mut self, _ctx: &mut FfiCtx<'_>, _mail: Mail<'_>) {}
}

aether_actor::export!(RootManager, Panel);
