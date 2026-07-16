//! `BloomeryChassis` — the coordinator chassis (ADR-0149 §Packaging).
//!
//! Assembled with the substrate builder like the hub, minus any render/audio/
//! window surface: `TraceDispatchCapability` (settlement + trace for local
//! dispatch), the `SQLite`-backed `StoreCapability`, `RpcServerCapability` (the
//! external typed-mail ingress the Demo dials), and a signal-blocking driver.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use aether_actor::Addressable;
use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::trace::TraceDispatchCapability;
use aether_kinds::BinaryManifest;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigError;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::artifacts::{ArtifactsCapability, ArtifactsConfig};
use crate::bloomery::MirrorDriverCapability;
use crate::bloomery::cli::BloomeryCli;
use crate::bloomery::driver::BloomeryDriverCapability;
use crate::bloomery::mirror::GithubMirrorConfig;
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
    /// The eviction-free artifacts content-store configuration.
    pub artifacts: ArtifactsConfig,
    /// The GitHub outward-mirror configuration driving the outbox consumer.
    /// Unconfigured (empty token/owner/repo) mounts the mirror driver disabled.
    pub mirror: GithubMirrorConfig,
}

impl BloomeryEnv {
    /// Resolve every knob from `AETHER_*` env (and literal defaults), with no
    /// argv overlay — the env-only entry point. Delegates to
    /// [`Self::from_env_with_argv`] with an empty [`BloomeryCli`], so the two
    /// paths resolve identically when argv is absent.
    ///
    /// # Errors
    ///
    /// See [`Self::from_env_with_argv`].
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_argv(&BloomeryCli::default())
    }

    /// ADR-0090 unit d: resolve every knob argv > `AETHER_*` env > default.
    /// `--rpc-port` shadows `AETHER_RPC_PORT` and `--store-path` shadows
    /// `AETHER_STORE_PATH`, each riding the derive-`Config` argv-then-env path
    /// (no naked env reads). Mirrors the hub's `HubEnv::from_env_with_argv`;
    /// takes `&BloomeryCli` by reference so the bin keeps `cli` for its
    /// `--describe` branch.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known env value (or argv overlay value)
    /// fails its parser.
    pub fn from_env_with_argv(cli: &BloomeryCli) -> Result<Self, ConfigError> {
        let rpc_port = RpcPortConfig::try_from_argv_then_env(cli.rpc.clone().into_layer())?.port;
        let store = StoreConfig::try_from_argv_then_env(cli.store.clone().into_layer())?;
        let artifacts = ArtifactsConfig::try_from_argv_then_env(cli.artifacts.clone().into_layer())?;
        // Env-only (argv > env > default with no argv overlay yet): the mirror
        // has no `--github-*` flags until an operator needs them, so it resolves
        // from `AETHER_GITHUB_*` / `GITHUB_TOKEN` and defaults to unconfigured.
        let mirror = GithubMirrorConfig::try_from_env()?;
        Ok(Self { rpc_port, store, artifacts, mirror })
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
    /// The `--describe` manifest (ADR-0115): the chassis profile, the mailbox
    /// namespaces this binary links, and the `build.rs` provenance. Bloomery is
    /// a minimal coordinator chassis — it links the trace dispatcher, the store,
    /// the artifacts record, and the RPC server — so it lists those directly.
    /// The hub's binary store forks `<binary> --describe` once at upload time to
    /// capture this.
    #[must_use]
    pub fn describe_manifest() -> BinaryManifest {
        let caps = vec![
            <TraceDispatchCapability as Addressable>::NAMESPACE.to_owned(),
            <StoreCapability as Addressable>::NAMESPACE.to_owned(),
            <ArtifactsCapability as Addressable>::NAMESPACE.to_owned(),
            <MirrorDriverCapability as Addressable>::NAMESPACE.to_owned(),
            <RpcServerCapability as Addressable>::NAMESPACE.to_owned(),
        ];
        BinaryManifest {
            chassis: Self::PROFILE.to_owned(),
            caps,
            git_sha: env!("AETHER_GIT_SHA").to_owned(),
            profile: env!("AETHER_BUILD_PROFILE").to_owned(),
            target: env!("AETHER_TARGET_TRIPLE").to_owned(),
        }
    }

    fn build_inner(env: BloomeryEnv) -> Result<BuiltChassis<Self>, BootError> {
        let BloomeryEnv { rpc_port, store, artifacts, mirror } = env;
        let boot = SubstrateBoot::builder("aether-bloomery", env!("CARGO_PKG_VERSION")).build()?;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        let rpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);

        // The driver owns the boot and drops it on the shutdown signal.
        let driver = BloomeryDriverCapability { boot };

        Builder::<Self>::new(registry, mailer)
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<StoreCapability>(store)
            .with_actor::<ArtifactsCapability>(artifacts)
            .with_actor::<MirrorDriverCapability>(mirror)
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
    use super::{ArtifactsConfig, BloomeryChassis, BloomeryEnv, Chassis, GithubMirrorConfig};
    use crate::store::StoreConfig;

    #[test]
    fn chassis_boots_and_claims_its_mailboxes() {
        // Port 0 → an OS-assigned ephemeral RPC port; the default `:memory:`
        // store touches no filesystem, and the artifacts store points at a temp
        // root so the test opens no data dir. The default (unconfigured) mirror
        // mounts the driver disabled — no timer, no network — so the chassis
        // boots clean without a token. A successful `build` boots every passive
        // (store, artifacts, mirror, trace, rpc) and claims each mailbox — a
        // claim conflict or a failed store open would surface as a `BootError`,
        // so `build` returning `Ok` is the assertion that the `aether.store`,
        // `aether.artifacts`, and `aether.bloomery.mirror` mailboxes were claimed.
        let artifacts_root = tempfile::tempdir().unwrap();
        let env = BloomeryEnv {
            rpc_port: 0,
            store: StoreConfig::default(),
            artifacts: ArtifactsConfig { root: Some(artifacts_root.path().to_str().unwrap().to_owned()) },
            mirror: GithubMirrorConfig::default(),
        };
        let chassis = BloomeryChassis::build(env).expect("bloomery chassis boots and claims its mailboxes");
        assert_eq!(BloomeryChassis::PROFILE, "bloomery");
        // Dropped without `run()` — teardown, no signal wait.
        drop(chassis);
    }
}
