//! Issue 7535: the first version of the hooks pair, whose replace hooks do
//! what the parent's config says (ADR-0249 §1, §2). A test republishes it with
//! a successor build of itself, or with `republish_hooks_v2`.
//!
//! - `Parent` (`test.republish.hooks.parent`, root) counts each `Bump`, holds
//!   a `HeldRequest`'s reply, and spawns one `Counter` in `wire`. Its
//!   `on_dehydrate` follows `HookFaultConfig::dehydrate`. Its `on_rehydrate`
//!   follows `successor_rehydrate` on an instance that has not dehydrated, a
//!   republish's successor, and `reinstated_rehydrate` on the instance that
//!   has, which is handed its own state back after a refused republish.
//! - `Counter` (`test.republish.hooks.counter`, an inline child) declares
//!   `type State`, so every dehydrate packs a child entry the successor has to
//!   rebuild.

use core::mem;

use aether_actor::{
    ActorInitError, Held, Pending, PriorState, Subname, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, WireCtx, actor,
};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DEHYDRATE_REFUSAL, HeldRequest, HeldRequestResult, HookFaultConfig, HookOutcome,
    REHYDRATE_REFUSAL,
};

/// What `Parent` carries across a republish: its count and the replies it
/// holds.
#[aether_data::kind(name = "aether.test_fixtures.republish_hooks_parent_state")]
struct ParentState {
    count: u32,
    held: Vec<Held<HeldRequestResult>>,
}

pub struct Parent {
    config: HookFaultConfig,
    count: u32,
    held: Vec<Held<HeldRequestResult>>,
    /// Set once `on_dehydrate` has run on this instance.
    dehydrated: bool,
}

#[actor(root, spawns(Counter))]
impl WasmActor for Parent {
    type Config = HookFaultConfig;
    const NAMESPACE: &'static str = "test.republish.hooks.parent";

    fn init(config: HookFaultConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Parent { config, count: 0, held: Vec::new(), dehydrated: false })
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.spawn_inline::<Counter>(Subname::Named("counter"), &())
            .map(drop)
            .map_err(|error| ActorInitError::new(format!("the counter does not spawn: {error:?}")))
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }

    /// Hold the reply and never answer it: a test reads what its requester
    /// receives when this instance closes.
    #[handler::request]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, _request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        self.held.push(held);
        pending
    }

    /// Save the count and the held replies, or do what the config says in
    /// their place. A save that fails gets the held replies back before the
    /// error is returned, so the instance that keeps running still holds them.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
        self.dehydrated = true;
        match self.config.dehydrate {
            HookOutcome::Succeeds => {}
            HookOutcome::Refuses => return Err(ActorInitError::new(DEHYDRATE_REFUSAL)),
            HookOutcome::Traps => panic!("the fixture was told to trap in on_dehydrate"),
        }

        let state = ParentState { count: self.count, held: mem::take(&mut self.held) };
        let saved = ctx.save_state_kind(0, &state);
        if saved.is_err() {
            self.held = state.held;
        }
        saved
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) -> Result<(), ActorInitError> {
        let outcome = if self.dehydrated {
            self.config.reinstated_rehydrate
        } else {
            self.config.successor_rehydrate
        };
        match outcome {
            HookOutcome::Succeeds => {}
            HookOutcome::Refuses => return Err(ActorInitError::new(REHYDRATE_REFUSAL)),
            HookOutcome::Traps => panic!("the fixture was told to trap in on_rehydrate"),
        }

        let ParentState { count, held } =
            prior.decode_kind::<ParentState>().ok_or("the parent's saved state does not decode")?;
        self.count = count;
        self.held = held;
        Ok(())
    }
}

/// Counts in the kind it saves, so its accessors move the state whole.
pub struct Counter {
    state: CountReport,
}

#[actor(instanced, child_of(Parent))]
impl WasmActor for Counter {
    const NAMESPACE: &'static str = "test.republish.hooks.counter";

    type State = CountReport;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Counter { state: CountReport { count: 0 } })
    }

    fn dehydrate(&self) -> CountReport {
        self.state.clone()
    }

    fn rehydrate(&mut self, state: CountReport) {
        self.state = state;
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.state.count += 1;
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        self.state.clone()
    }
}

aether_actor::export!(public = [Parent], private = [Counter]);
