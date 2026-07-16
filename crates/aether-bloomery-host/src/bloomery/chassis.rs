//! `BloomeryChassis` — the coordinator chassis (ADR-0149 §Packaging).
//!
//! Assembled with the substrate builder like the hub, minus any render/audio/
//! window surface: `TraceDispatchCapability` (settlement + trace for local
//! dispatch), the `SQLite`-backed `StoreCapability`, `RpcServerCapability` (the
//! external typed-mail ingress the Demo dials), and a signal-blocking driver.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::trace::TraceDispatchCapability;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigError;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::bloomery::driver::BloomeryDriverCapability;
use crate::store::{StoreCapability, StoreConfig};

/// The default RPC port when `AETHER_RPC_PORT` is unset (distinct from the hub's
/// 8901 so a bloomery and a hub can coexist on one host).
pub const DEFAULT_RPC_PORT: u16 = 8909;

/// The RPC ingress port knob, resolved argv > `AETHER_RPC_PORT` > default.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_RPC", cli_prefix = "rpc")]
pub struct RpcPortConfig {
    /// The localhost port `RpcServerCapability` binds. The engines cap injects
    /// `AETHER_RPC_PORT` when it forks a bloomery, so this resolves it.
    #[config(default = 8909)]
    pub port: u16,
}

impl Default for RpcPortConfig {
    fn default() -> Self {
        Self { port: DEFAULT_RPC_PORT }
    }
}

/// The unit marker for the Bloomery chassis (ADR-0071).
pub struct BloomeryChassis;

/// The resolved boot knobs for [`BloomeryChassis`].
#[derive(Clone, Debug)]
pub struct BloomeryEnv {
    /// The localhost RPC ingress port.
    pub rpc_port: u16,
    /// The `SQLite` journal store configuration.
    pub store: StoreConfig,
}

impl BloomeryEnv {
    /// Resolve every knob from `AETHER_*` env (and literal defaults). Each knob
    /// rides the ADR-0090 derive-`Config` path — no naked env reads.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known env value fails its parser.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self { rpc_port: RpcPortConfig::try_from_env()?.port, store: StoreConfig::try_from_env()? })
    }
}

impl Chassis for BloomeryChassis {
    const PROFILE: &'static str = "bloomery";
    type Driver = BloomeryDriverCapability;
    type Env = BloomeryEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
    }
}

impl BloomeryChassis {
    fn build_inner(env: BloomeryEnv) -> Result<BuiltChassis<Self>, BootError> {
        let BloomeryEnv { rpc_port, store } = env;
        let boot = SubstrateBoot::builder("aether-bloomery", env!("CARGO_PKG_VERSION")).build()?;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        let rpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);

        // The driver owns the boot and drops it on the shutdown signal.
        let driver = BloomeryDriverCapability { boot };

        Builder::<Self>::new(registry, mailer)
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<StoreCapability>(store)
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: rpc_addr.to_string(),
                peer_kind: PeerKind::Substrate {
                    engine_name: "aether-bloomery".into(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
            })
            .driver(driver)
            .build()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{BloomeryChassis, BloomeryEnv, Chassis};
    use crate::store::StoreConfig;

    #[test]
    fn chassis_boots_and_claims_its_mailboxes() {
        // Port 0 → an OS-assigned ephemeral RPC port; the default `:memory:`
        // store touches no filesystem. A successful `build` boots every passive
        // (store, trace, rpc) and claims each mailbox — a claim conflict or a
        // failed store open would surface as a `BootError`, so `build` returning
        // `Ok` is the assertion that the `aether.store` mailbox was claimed.
        let env = BloomeryEnv { rpc_port: 0, store: StoreConfig::default() };
        let chassis = BloomeryChassis::build(env).expect("bloomery chassis boots and claims its mailboxes");
        assert_eq!(BloomeryChassis::PROFILE, "bloomery");
        // Dropped without `run()` — teardown, no signal wait.
        drop(chassis);
    }
}
