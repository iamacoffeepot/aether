//! Chassis-agnostic probe fixture (issue #6507).
//!
//! `QuietProbe` reports to no observer, so it loads on every chassis: the
//! headless and `FleetHarness` tests select it by export
//! (`export: Some("test.quiet_probe")`). It carries two behaviours:
//!
//! - The ADR-0163 §3 asset-window pull: `wire` pulls the bundle's
//!   `asset_fixture.txt` and stashes a fingerprint, which an `AssetProbe`
//!   reads back after the window has closed.
//! - On every `LogMarker` delivery, a `tracing::info!("typed_send_alive")`
//!   that flows through the actor-aware subscriber (issue #581) into the
//!   per-actor log ring the log-ring tests read.

#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and `on_log_marker` only logs, so it reads no state — the same allow `probe` carries

use aether_actor::{ActorInitError, AssetWindow, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{AssetProbe, AssetProbeResult, LogMarker};

pub struct QuietProbe {
    /// ADR-0163 §3 (#3984): what `wire` pulled from the asset load window,
    /// surfaced later through [`QuietProbe::on_asset_probe`].
    asset: AssetProbeResult,
}

#[actor(root)]
impl WasmActor for QuietProbe {
    const NAMESPACE: &'static str = "test.quiet_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(QuietProbe { asset: AssetProbeResult::default() })
    }

    /// Pull the bundle's asset through the load window (open during `wire`).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_, Self>) {
        // ADR-0163 §3 (#3984): stash a content fingerprint — length + a
        // wrapping-sum checksum — so a later `AssetProbe` proves the
        // guest-side pull round-tripped the exact bytes across the FFI and
        // that the value survived the window closing after `wire`.
        if let Some(bytes) = ctx.asset("asset_fixture.txt") {
            let checksum = bytes.iter().fold(0u64, |acc, &byte| acc.wrapping_add(u64::from(byte)));
            self.asset = AssetProbeResult { pulled: true, len: bytes.len() as u64, checksum };
        }
    }

    /// Emits `typed_send_alive` on every delivery.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.log_marker`; read the line back with
    /// `actor_logs`.
    #[handler::single]
    fn on_log_marker(&mut self, _ctx: &mut WasmCtx<'_>, _: LogMarker) {
        tracing::info!(target: "aether_test_fixture_probe", "typed_send_alive");
    }

    /// ADR-0163 §3 (#3984): reply with the fingerprint of the asset this
    /// fixture pulled from its load window during `wire`. Runs post-`wire`
    /// (the window has closed), so a non-zero `pulled` reply proves the
    /// guest-side `AssetWindow::asset` pull worked while the window was open
    /// and the bytes survived into the instance's ordinary state.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.asset_probe`; the reply
    /// `aether.test_fixtures.asset_probe_result` carries `{ pulled, len,
    /// checksum }`.
    #[handler::single]
    fn on_asset_probe(&mut self, _ctx: &mut WasmCtx<'_>, _query: AssetProbe) -> AssetProbeResult {
        self.asset.clone()
    }
}
