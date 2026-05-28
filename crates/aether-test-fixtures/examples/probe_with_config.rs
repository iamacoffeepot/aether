//! ADR-0090 c1 typed-config fixture. Exercises the
//! `FfiActor::Config = ProbeConfig` path end-to-end: the host writes
//! postcard-encoded `ProbeConfig` bytes at `CONFIG_OFFSET` during
//! `Component::instantiate`; the guest's `init_v2_p32` shim decodes
//! them via `<ProbeConfig as Kind>::decode_from_bytes` and threads
//! the typed struct into `Probe::init(config, ctx)`.
//!
//! The fixture stashes `(seed, label)` at boot and replies with a
//! `ConfigEcho` on every `ConfigQuery` mail so a test can assert the
//! config round-tripped intact. No tick / render behaviour — the
//! sibling `probe` covers that.

use aether_actor::{BootError, FfiActor, FfiCtx, OutboundReply, Resolver, actor};
use aether_test_fixtures::{ConfigEcho, ConfigQuery, ProbeConfig};

pub struct ProbeWithConfig {
    seed: u32,
    label: String,
}

#[actor]
impl FfiActor for ProbeWithConfig {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "test_fixtures_probe_with_config";

    fn init<C>(config: ProbeConfig, _ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(ProbeWithConfig {
            seed: config.seed,
            label: config.label,
        })
    }

    /// Reply with a `ConfigEcho` describing the cached config. Lets
    /// the integration test observe what the typed `init` actually
    /// received without scraping logs or readback.
    #[handler]
    fn on_config_query(&mut self, ctx: &mut FfiCtx<'_>, _query: ConfigQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&ConfigEcho {
                seed: self.seed,
                label: self.label.clone(),
            });
        }
    }
}

aether_actor::export!(ProbeWithConfig);
