//! SPIKE (issue 7414): an asset bundle component. `wire` walks the asset
//! catalog, fetches one asset at a time by copy, folds it into a sum, and
//! drops it, so the instance keeps none of the payload.

#![allow(clippy::needless_pass_by_value)]

use aether_actor::{ActorInitError, AssetCatalog, AssetWindow, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};

/// Ask a loaded bundle what it read. Reply: [`BundleSum`].
#[aether_data::kind(name = "spike.bundle.sum_query", copy, no_serde)]
pub struct BundleSumQuery;

/// What a bundle read in its load window.
#[aether_data::kind(name = "spike.bundle.sum", copy, eq, no_serde)]
pub struct BundleSum {
    pub sum: u64,
    pub assets: u32,
    pub bytes: u64,
}

pub struct Bundle {
    read: BundleSum,
}

#[actor(instanced, root)]
impl WasmActor for Bundle {
    const NAMESPACE: &'static str = "spike.bundle";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { read: BundleSum { sum: 0, assets: 0, bytes: 0 } })
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        for index in 0..ctx.assets().len() {
            let name = ctx.assets()[index].name.clone();
            let Some(asset) = ctx.asset(&name) else {
                continue;
            };

            self.read.sum = asset.iter().fold(self.read.sum, |sum, &byte| sum.wrapping_add(u64::from(byte)));
            self.read.assets += 1;
            self.read.bytes += asset.len() as u64;
        }
    }

    #[handler::request]
    fn on_sum_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: BundleSumQuery) -> BundleSum {
        self.read
    }
}

aether_actor::export!(public = [Bundle]);
