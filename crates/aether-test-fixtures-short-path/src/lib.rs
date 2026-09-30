//! ADR-0241 §5 fixture: guest children a short path reaches.
//!
//! Each parent here is named in exactly one instanced child type's
//! `child_of(..)`, so a hole beneath it names that child:
//!
//! - `Trunk` (exported root) spawns the private inline `Branch` as `branch`
//!   when bumped, and `Branch` spawns the private inline `Leaf` as `leaf` in
//!   its `wire`, so `test.short_path.trunk/:branch/:leaf` crosses two private
//!   placements. The spawn runs in a handler, so the alias batches ride the
//!   bump's chain and a settled bump has committed them.
//! - `Placed` (exported, instanced) declares `child_of(Host)`, so a
//!   `load_under` beneath the exported root `Host` lands at
//!   `test.short_path.host/test.short_path.placed:NAME`.
//!
//! Every type counts the bumps it receives, so each handler reads its state.

#![forbid(unsafe_code)]

use aether_actor::{ActorInitError, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::Bump;

/// Exported root: spawns `Branch` on its first bump.
pub struct Trunk {
    bumps: u32,
}

#[actor(root, spawns(Branch))]
impl WasmActor for Trunk {
    const NAMESPACE: &'static str = "test.short_path.trunk";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Trunk { bumps: 0 })
    }

    #[handler::tell]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        if self.bumps == 0 {
            let _ = ctx.spawn_inline_child::<Trunk, Branch>(Subname::Named("branch"), &());
        }
        self.bumps += 1;
    }
}

/// Private inline child of `Trunk`: spawns `Leaf` once wired.
pub struct Branch {
    bumps: u32,
}

#[actor(instanced, child_of(Trunk), spawns(Leaf))]
impl WasmActor for Branch {
    const NAMESPACE: &'static str = "test.short_path.branch";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Branch { bumps: 0 })
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        let _ = ctx.spawn_inline_child::<Branch, Leaf>(Subname::Named("leaf"), &());
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

/// Private inline child of `Branch`.
pub struct Leaf {
    bumps: u32,
}

#[actor(instanced, child_of(Branch))]
impl WasmActor for Leaf {
    const NAMESPACE: &'static str = "test.short_path.leaf";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Leaf { bumps: 0 })
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

/// Exported root a `Placed` guest loads beneath.
pub struct Host {
    bumps: u32,
}

#[actor(root)]
impl WasmActor for Host {
    const NAMESPACE: &'static str = "test.short_path.host";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Host { bumps: 0 })
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

/// Exported instanced child a `load_under` places beneath `Host`.
pub struct Placed {
    bumps: u32,
}

#[actor(instanced, child_of(Host))]
impl WasmActor for Placed {
    const NAMESPACE: &'static str = "test.short_path.placed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Placed { bumps: 0 })
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

aether_actor::export!(public = [Trunk, Host, Placed], private = [Branch, Leaf]);
