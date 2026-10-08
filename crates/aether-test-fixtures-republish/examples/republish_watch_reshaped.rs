//! Issue 7496: the watch family's namespaces and rows, with the ledger's
//! watch context reshaped, republishing `republish_watch_v1`.
//!
//! `Ledger`'s `WatchNote` gains a `generation` field, so its `Kind::ID`
//! changes, and a republish while a v1 ledger holds a watch carries a context
//! of a kind this module does not declare. It must be refused (ADR-0139 §4,
//! ADR-0079 §8).
//!
//! Nothing here is ever installed, so each handler does nothing. This module
//! names no type of its crate's lib: a module declares the kinds linked into
//! it, and the lib's ledger declares the v1 `WatchNote`.

use aether_actor::{ActorInitError, Departed, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    SubstrateHarnessObserver, WatchAdmit, WatchAdmitResult, WatchAuditor, WatchClerkSpawn, WatchHeld, WatchHold,
    WatchLedgerConfig, WatchLedgerQuery, WatchLedgerReport, WatchNudge, WatchPeerAdmit, WatchPeerConfig, WatchProvider,
    WatchRelease,
};

/// v1's watch context, name kept, with an added field.
#[aether_data::kind(name = "aether.test_fixtures.republish_watch_note", no_serde)]
pub struct WatchNote {
    tag: u32,
    generation: u32,
}

/// The shared `WatchLedger`'s rows, with the provider handler taking the
/// reshaped context.
pub struct Ledger;

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for Ledger {
    type Config = WatchLedgerConfig;
    const NAMESPACE: &'static str = "test.republish.watch.ledger";

    fn init(_config: WatchLedgerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Ledger)
    }

    #[handler::request]
    fn on_admit(&mut self, _ctx: &mut WasmCtx<'_>, _admit: WatchAdmit) -> WatchAdmitResult {
        unwatched()
    }

    #[handler::tell]
    fn on_hold(&mut self, _ctx: &mut WasmCtx<'_>, _hold: WatchHold) {}

    #[handler::request]
    fn on_held(&mut self, _ctx: &mut WasmCtx<'_>, _held: WatchHeld) -> WatchAdmitResult {
        unwatched()
    }

    #[handler::tell]
    fn on_release(&mut self, _ctx: &mut WasmCtx<'_>, _release: WatchRelease) {}

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: WatchLedgerQuery) -> WatchLedgerReport {
        WatchLedgerReport::default()
    }

    #[handler::event]
    fn on_provider_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<WatchProvider>, _note: WatchNote) {}

    #[handler::event]
    fn on_auditor_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<WatchAuditor>) {}
}

fn unwatched() -> WatchAdmitResult {
    WatchAdmitResult::Err { error: "the reshaped ledger watches nothing".to_owned() }
}

/// The shared `WatchPeer`'s rows.
pub struct Peer;

#[actor(root, depends(Ledger))]
impl WasmActor for Peer {
    type Config = WatchPeerConfig;
    const NAMESPACE: &'static str = "test.republish.watch.peer";

    fn init(_config: WatchPeerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Peer)
    }

    #[handler::tell]
    fn on_admit(&mut self, _ctx: &mut WasmCtx<'_>, _admit: WatchPeerAdmit) {}

    #[handler::response]
    fn on_admitted(&mut self, _ctx: &mut WasmCtx<'_>, _result: WatchAdmitResult) {}

    #[handler::tell]
    fn on_nudge(&mut self, _ctx: &mut WasmCtx<'_>, _nudge: WatchNudge) {}
}

/// The shared `WatchDesk`'s row. It declares the clerk, as a successor must
/// keep its predecessor's private child type, and spawns none.
pub struct Desk;

#[actor(instanced, root, spawns(Clerk))]
impl WasmActor for Desk {
    const NAMESPACE: &'static str = "test.republish.watch.desk";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Desk)
    }

    #[handler::tell]
    fn on_spawn(&mut self, _ctx: &mut WasmCtx<'_>, _spawn: WatchClerkSpawn) {}
}

/// The shared `WatchClerk`'s namespace and rows.
pub struct Clerk;

#[actor(instanced, child_of(Desk), depends(SubstrateHarnessObserver))]
impl WasmActor for Clerk {
    const NAMESPACE: &'static str = "test.republish.watch.clerk";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Clerk)
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: WatchLedgerQuery) -> WatchLedgerReport {
        WatchLedgerReport::default()
    }

    #[handler::event]
    fn on_desk_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Desk>, _note: WatchNote) {}
}

aether_actor::export!(public = [Ledger, Peer, Desk], private = [Clerk]);
