//! Failing-`wire` fixtures (issue #7463, ADR-0247 rule 3).
//!
//! `WireFault` mails the harness observer a [`WireMarker`] from `wire` and
//! then does what its config says: returns `Ok`, returns an error, or traps.
//! A birth holds the mail its `wire` sent until it goes live, so the marker
//! reaches the observer only for a birth that succeeded. It declares the
//! observer, so it loads only on the `SubstrateHarness`.
//!
//! `WireRefuser` declares nothing and takes no config: its `wire` always
//! returns an error, so a boot list on any chassis can hold it.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::{
    LogMarker, SubstrateHarnessObserver, WIRE_REFUSAL, WireFaultConfig, WireMarker, WireOutcome,
};

pub struct WireFault {
    outcome: WireOutcome,
}

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for WireFault {
    type Config = WireFaultConfig;
    const NAMESPACE: &'static str = "test.wire_fault";

    fn init(config: WireFaultConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WireFault { outcome: config.outcome })
    }

    /// Mail the marker, then succeed, refuse, or trap as configured.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.send::<SubstrateHarnessObserver>(&WireMarker);
        match self.outcome {
            WireOutcome::Succeeds => Ok(()),
            WireOutcome::Refuses => Err(ActorInitError::new(WIRE_REFUSAL)),
            WireOutcome::Traps => panic!("the fixture was told to trap in wire"),
        }
    }

    /// Emits `wire_fault_alive`, so a live instance is observable.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.log_marker`; read the line back with
    /// `actor_logs`.
    #[handler::tell]
    fn on_log_marker(&mut self, _ctx: &mut WasmCtx<'_>, _: LogMarker) {
        tracing::info!(target: "aether_test_fixture_wire_fault", "wire_fault_alive");
    }
}

pub struct WireRefuser;

#[actor(root)]
impl WasmActor for WireRefuser {
    const NAMESPACE: &'static str = "test.wire_refuser";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WireRefuser)
    }

    /// Always refuses.
    fn wire(&mut self, _ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        Err(ActorInitError::new(WIRE_REFUSAL))
    }

    /// Never reached: no `WireRefuser` goes live.
    ///
    /// # Agent
    /// Not sendable: this fixture never finishes its birth.
    #[handler::tell]
    fn on_log_marker(&mut self, _ctx: &mut WasmCtx<'_>, _: LogMarker) {
        tracing::info!(target: "aether_test_fixture_wire_fault", "wire_refuser_alive");
    }
}
