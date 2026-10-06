//! Issue 7496: the second version of the watch family, republishing
//! `republish_watch_v1`.
//!
//! - `WatchLedger` and `WatchDesk`, with its inline `WatchClerk`, are the
//!   crate's shared types, so a watch a v1 guest made is handled by the same
//!   code under a new module.
//! - `Peer` keeps the shared `WatchPeer`'s rows and saved state at its
//!   namespace. With `WatchPeerConfig::trap_on_rehydrate` set, its
//!   `on_rehydrate` traps, so a republish of the module fails after every
//!   other member answered ready.

use std::process;

use aether_actor::{ActorInitError, PriorState, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CountReport, WatchAdmit, WatchAdmitResult, WatchNudge, WatchPeerAdmit, WatchPeerConfig, WatchThrough,
};
use aether_test_fixtures_republish::{WatchClerk, WatchDesk, WatchLedger};

pub struct Peer {
    config: WatchPeerConfig,
    admits: u32,
}

#[actor(root, depends(WatchLedger))]
impl WasmActor for Peer {
    type Config = WatchPeerConfig;
    const NAMESPACE: &'static str = "test.republish.watch.peer";

    fn init(config: WatchPeerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Peer { config, admits: 0 })
    }

    fn unwire(&mut self, _ctx: &mut WasmCtx<'_>) {
        assert!(!self.config.trap_on_unwire, "the fixture was told to trap in unwire");
    }

    /// Hand-written, where v1 generates it from `type State`, because the
    /// rehydrate side must be able to trap.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        ctx.save_state_kind::<CountReport>(0, &CountReport { count: self.admits });
    }

    /// Restore v1's count, or, with `trap_on_rehydrate` set, trap: `abort`
    /// lowers to `unreachable`, which the host reports as an `on_rehydrate`
    /// failure.
    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        if self.config.trap_on_rehydrate {
            process::abort();
        }
        if let Some(saved) = prior.decode_kind::<CountReport>() {
            self.admits = saved.count;
        }
    }

    #[handler::tell]
    fn on_admit(&mut self, ctx: &mut WasmCtx<'_>, admit: WatchPeerAdmit) {
        ctx.send::<WatchLedger>(&WatchAdmit { tag: admit.tag, through: WatchThrough::Provider });
        self.admits += 1;
    }

    #[handler::response]
    fn on_admitted(&mut self, _ctx: &mut WasmCtx<'_>, _result: WatchAdmitResult) {}

    #[handler::tell]
    fn on_nudge(&mut self, _ctx: &mut WasmCtx<'_>, _nudge: WatchNudge) {}
}

aether_actor::export!(public = [WatchLedger, Peer, WatchDesk], private = [WatchClerk]);
