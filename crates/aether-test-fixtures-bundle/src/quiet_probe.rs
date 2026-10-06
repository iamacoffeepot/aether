//! Chassis-agnostic probe fixture (issue #6507).
//!
//! `QuietProbe` reports to no observer, so it loads on every chassis: the
//! headless and `FleetHarness` tests select it by export
//! (`export: Some("test.quiet_probe")`). It carries two behaviours:
//!
//! - The ADR-0163 §3 asset-window pull: `wire` pulls the bundle's
//!   `asset_fixture.txt` and stashes a fingerprint, which an `AssetProbe`
//!   reads back after the window has closed. It also takes the same asset
//!   as a blob and keeps it, which an `AssetBlobProbe` is answered with.
//! - On every `LogMarker` delivery, a `tracing::info!("typed_send_alive")`
//!   that flows through the actor-aware subscriber (issue #581) into the
//!   per-actor log ring the log-ring tests read.

use aether_actor::{ActorInitError, AssetWindow, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::{Blob, BlobReader};
use aether_test_fixtures_kinds::{
    AssetBlobProbe, AssetBlobProbeResult, AssetProbe, AssetProbeResult, EmptyAssetProbe, EmptyAssetProbeResult,
    LogMarker,
};

pub struct QuietProbe {
    /// ADR-0163 §3 (#3984): what `wire` pulled from the asset load window,
    /// surfaced later through [`QuietProbe::on_asset_probe`].
    asset: AssetProbeResult,
    /// The same asset as `wire` took it through `AssetWindow::asset_blob`,
    /// held by handle past the window and forwarded by
    /// [`QuietProbe::on_asset_blob_probe`].
    asset_blob: Option<Blob>,
    /// What `wire` got for `asset_empty.bin` by each verb, answered by
    /// [`QuietProbe::on_empty_asset_probe`].
    empty: EmptyAssetProbeResult,
}

#[actor(root)]
impl WasmActor for QuietProbe {
    const NAMESPACE: &'static str = "test.quiet_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(QuietProbe {
            asset: AssetProbeResult::default(),
            asset_blob: None,
            empty: EmptyAssetProbeResult { copied_len: None, blob_len: None },
        })
    }

    /// Pull the bundle's asset through the load window (open during `wire`).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_, Self>) {
        // The blob verb goes first, so it is the call that traps for an
        // instance spawned without its module's bytes, whose window has no code.
        self.asset_blob = ctx.asset_blob("asset_fixture.txt");

        // ADR-0163 §3 (#3984): stash a content fingerprint — length + a
        // wrapping-sum checksum — so a later `AssetProbe` proves the
        // guest-side pull round-tripped the exact bytes across the FFI and
        // that the value survived the window closing after `wire`.
        if let Some(bytes) = ctx.asset("asset_fixture.txt") {
            let checksum = bytes.iter().fold(0u64, |acc, &byte| acc.wrapping_add(u64::from(byte)));
            self.asset = AssetProbeResult { pulled: true, len: bytes.len() as u64, checksum };
        }

        // After the two fetches above, so the sourceless-window test still
        // traps on the first `asset_blob`. The bundle carries no such asset
        // unless a test appends one, so both lengths are `None` otherwise.
        self.empty = EmptyAssetProbeResult {
            blob_len: ctx.asset_blob("asset_empty.bin").map(|blob| BlobReader::open(&blob).len()),
            copied_len: ctx.asset("asset_empty.bin").map(|bytes| bytes.len() as u64),
        };
    }

    /// Emits `typed_send_alive` on every delivery.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.log_marker`; read the line back with
    /// `actor_logs`.
    #[handler::tell]
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
    #[handler::request]
    fn on_asset_probe(&mut self, _ctx: &mut WasmCtx<'_>, _query: AssetProbe) -> AssetProbeResult {
        self.asset.clone()
    }

    /// Reply with the asset blob this fixture took from its load window
    /// during `wire` and kept. Runs post-`wire`, so the reply forwards a
    /// handle that outlived the window, and its bytes never entered this
    /// guest's memory.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.asset_blob_probe`; the reply
    /// `aether.test_fixtures.asset_blob_probe_result` carries `{ blob }`.
    #[handler::request]
    fn on_asset_blob_probe(&mut self, _ctx: &mut WasmCtx<'_>, _query: AssetBlobProbe) -> AssetBlobProbeResult {
        AssetBlobProbeResult { blob: self.asset_blob.clone() }
    }

    /// Reply with the length each asset verb returned for `asset_empty.bin`
    /// during `wire`.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.empty_asset_probe`; the reply
    /// `aether.test_fixtures.empty_asset_probe_result` carries `{ copied_len,
    /// blob_len }`.
    #[handler::request]
    fn on_empty_asset_probe(&mut self, _ctx: &mut WasmCtx<'_>, _query: EmptyAssetProbe) -> EmptyAssetProbeResult {
        self.empty.clone()
    }
}
