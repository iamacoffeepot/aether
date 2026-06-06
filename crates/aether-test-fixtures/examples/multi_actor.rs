//! ADR-0096 fixture: a multi-actor module. Two `FfiActor` types in one
//! crate, exported together via `export!(RootManager, Panel)`. Proves
//! multi-type coexistence in a single wasm module (no duplicate-symbol
//! collision, which ADR-0014 §4 previously forbade) and that the entry
//! type (the first export, `RootManager`) loads through an unmodified
//! host. Selecting the non-entry export (`Panel`) is the follow-on PR.

// `#[handler]` methods take `&mut self` to match the dispatch ABI even
// when stateless.
#![allow(clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, Resolver, actor};
use aether_kinds::Ping;

/// Entry export — the first type in the `export!` list. An unmodified
/// host instantiates this one.
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

/// Sibling export — reachable once the host can resolve an export
/// selector to its actor-type tag (follow-on PR). Present here to prove
/// two `#[actor]` types coexist in one module.
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
}

aether_actor::export!(RootManager, Panel);
