//! ADR-0230 fixture: an inline child with a declared dependency.
//!
//! `Holder` is the module's export. Its `wire` spawns its private child
//! `Needy` inline, and `Needy` declares `depends(ClipboardCapability)`. The module
//! loads whether or not a clipboard actor is live: dependencies are checked
//! where an actor stands up (ADR-0241 §4), and for `Needy` that is its spawn.
//! `Holder` keeps how the spawn ended and answers it to a
//! `SpawnOutcomeQuery`, so a test reads the guest's own `SpawnError`.

#![forbid(unsafe_code)]
#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and `Needy` is stateless — the same allow the defaultless fixture carries

use aether_actor::{ActorInitError, SpawnError, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_clipboard::ClipboardCapability;
use aether_kinds::Ping;
use aether_test_fixtures_kinds::{SpawnOutcome, SpawnOutcomeQuery};

/// The module's export: spawns `Needy` inline once wired, and keeps how the
/// spawn ended.
pub struct Holder {
    outcome: SpawnOutcome,
}

#[actor(root, spawns(Needy))]
impl WasmActor for Holder {
    const NAMESPACE: &'static str = "test.inline_dependency.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Holder { outcome: SpawnOutcome::default() })
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        let spawned = ctx.spawn_inline_child::<Holder, Needy>(Subname::Named("needy"), &());
        self.outcome = SpawnOutcome {
            spawned: spawned.is_ok(),
            dependency_not_live: matches!(spawned, Err(SpawnError::DependencyNotLive)),
        };
    }

    #[handler::single]
    fn on_spawn_outcome_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: SpawnOutcomeQuery) -> SpawnOutcome {
        self.outcome
    }
}

/// The private inline child whose declared dependency the host checks when
/// `Holder` spawns it.
pub struct Needy;

#[actor(instanced, child_of(Holder), depends(ClipboardCapability))]
impl WasmActor for Needy {
    const NAMESPACE: &'static str = "test.inline_dependency.needy";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Needy)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

aether_actor::export!(public = [Holder], private = [Needy]);
