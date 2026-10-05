//! SPIKE (branch `spike/load-drop-heap`, issue 7414): `QuietProbe`'s asset
//! pull on an instanced type, so one module's bytes load and drop in a loop
//! (a singleton's name retires at its first drop).

use aether_actor::{ActorInitError, AssetWindow, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::Blob;
use aether_test_fixtures_kinds::LogMarker;

pub struct SpikeAssetProbe {
    asset_blob: Option<Blob>,
    checksum: u64,
}

#[actor(instanced, root)]
impl WasmActor for SpikeAssetProbe {
    const NAMESPACE: &'static str = "test.spike.asset_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SpikeAssetProbe { asset_blob: None, checksum: 0 })
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_, Self>) {
        self.asset_blob = ctx.asset_blob("asset_fixture.txt");
        if let Some(bytes) = ctx.asset("asset_fixture.txt") {
            self.checksum = bytes.iter().fold(0u64, |acc, &byte| acc.wrapping_add(u64::from(byte)));
        }
    }

    #[handler::tell]
    fn on_log_marker(&mut self, _ctx: &mut WasmCtx<'_>, _: LogMarker) {
        tracing::info!(target: "aether_test_fixture_probe", checksum = self.checksum, "spike_asset_probe");
    }
}
