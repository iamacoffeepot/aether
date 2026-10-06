//! Instanced asset-window fixture (issue #7461).
//!
//! `AssetInstance` is the bundle's one instanced type that reads an asset:
//! `wire` pulls `asset_fixture.txt` through its load window and keeps a
//! fingerprint, which an `AssetProbe` reads back after the window closed.
//! Each instance has its own load window (ADR-0163 §4), so two instances of
//! it prove that every spawn of a replicated boot entry brought the module's
//! bytes, where the singleton `QuietProbe` can prove only one.

use aether_actor::{ActorInitError, AssetWindow, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::{AssetProbe, AssetProbeResult};

pub struct AssetInstance {
    /// What `wire` pulled from this instance's load window.
    asset: AssetProbeResult,
}

#[actor(instanced, root)]
impl WasmActor for AssetInstance {
    const NAMESPACE: &'static str = "test.asset_instance";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(AssetInstance { asset: AssetProbeResult::default() })
    }

    /// Pull the bundle's asset through the load window, which is open
    /// during `wire`, and keep the fingerprint `QuietProbe` computes: the
    /// length and a wrapping-sum checksum of the bytes.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_, Self>) -> Result<(), ActorInitError> {
        if let Some(bytes) = ctx.asset("asset_fixture.txt") {
            let checksum = bytes.iter().fold(0u64, |sum, &byte| sum.wrapping_add(u64::from(byte)));
            self.asset = AssetProbeResult { pulled: true, len: bytes.len() as u64, checksum };
        }
        Ok(())
    }

    /// Reply with the fingerprint of the asset this instance pulled from
    /// its own load window during `wire`.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.asset_probe`; the reply
    /// `aether.test_fixtures.asset_probe_result` carries `{ pulled, len,
    /// checksum }`.
    #[handler::request]
    fn on_asset_probe(&mut self, _ctx: &mut WasmCtx<'_>, _query: AssetProbe) -> AssetProbeResult {
        self.asset.clone()
    }
}
