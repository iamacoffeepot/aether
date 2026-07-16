//! `BloomeryChassis` — the coordinator chassis (ADR-0149 §Packaging).
//!
//! Assembled with the substrate builder like the hub, minus any render/audio/
//! window surface: `TraceDispatchCapability` (settlement + trace for local
//! dispatch), the `SQLite`-backed `StoreCapability`, `RpcServerCapability` (the
//! external typed-mail ingress the Demo dials), and a signal-blocking driver.

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use aether_actor::Addressable;
use aether_capabilities::http::{HttpServerCapability, HttpServerConfig};
use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::trace::TraceDispatchCapability;
use aether_capabilities::{ComponentHostCapability, ComponentHostConfig};
use aether_data::{Kind, mailbox_id_from_name};
use aether_kinds::{BinaryManifest, LoadComponent};
use aether_substrate::Mail;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigError;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::api::BloomeryApiCapability;
use crate::artifacts::{ArtifactsCapability, ArtifactsConfig};
use crate::bloomery::MirrorDriverCapability;
use crate::bloomery::cli::BloomeryCli;
use crate::bloomery::driver::BloomeryDriverCapability;
use crate::bloomery::mirror::GithubMirrorConfig;
use crate::source::{SourceCapability, SourceConfig};
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

/// The default REST control-API port when `AETHER_HTTP_PORT` is unset —
/// distinct from the RPC port so the two ingresses coexist on one host.
pub const DEFAULT_HTTP_PORT: u16 = 8910;

/// The REST control-API ingress port knob, resolved argv > `AETHER_HTTP_PORT` >
/// default. The `aether.http.server` cap binds this on localhost; the operator
/// drives the bloom lifecycle over it with `curl` (ADR-0149 §Packaging, #3498).
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_HTTP", cli_prefix = "http")]
pub struct HttpPortConfig {
    /// The localhost port the REST control API binds.
    #[config(default = 8910)]
    pub port: u16,
}

impl Default for HttpPortConfig {
    fn default() -> Self {
        Self { port: DEFAULT_HTTP_PORT }
    }
}

/// The boot-autoload knob (ADR-0149 §Packaging), resolved argv > env > default.
/// When set, the chassis autoloads the control-core wasm actor at boot so the
/// coordinator comes up as a running control loop with no external
/// `aether.component.load` step. Unset → the bin boots the passive caps only and
/// the control core is loaded on demand over RPC. Bloomery links exactly one
/// autoloadable component, so this is a single wasm path rather than a
/// multi-entry manifest.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_CONTROL_CORE", cli_prefix = "control-core")]
pub struct ControlCoreConfig {
    /// Path to the control-core component wasm to autoload at boot
    /// (`AETHER_CONTROL_CORE_WASM` / `--control-core-wasm`). Unset → no autoload.
    #[config(cli_long = "control-core-wasm")]
    pub wasm: Option<String>,
}

/// The unit marker for the Bloomery chassis (ADR-0071).
pub struct BloomeryChassis;

/// The resolved boot knobs for [`BloomeryChassis`].
#[derive(Clone, Debug)]
pub struct BloomeryEnv {
    /// The localhost RPC ingress port.
    pub rpc_port: u16,
    /// The localhost REST control-API ingress port.
    pub http_port: u16,
    /// The `SQLite` journal store configuration.
    pub store: StoreConfig,
    /// The eviction-free artifacts content-store configuration.
    pub artifacts: ArtifactsConfig,
    /// The GitHub outward-mirror configuration driving the outbox consumer.
    /// Unconfigured (empty token/owner/repo) mounts the mirror driver disabled.
    pub mirror: GithubMirrorConfig,
    /// The git source-port capability's GitHub connection configuration.
    pub source: SourceConfig,
    /// Path to the control-core component wasm to autoload at boot; unset → no
    /// autoload (the control core is loaded on demand over RPC).
    pub control_core_wasm: Option<String>,
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
        let http_port = HttpPortConfig::try_from_argv_then_env(cli.http.clone().into_layer())?.port;
        let store = StoreConfig::try_from_argv_then_env(cli.store.clone().into_layer())?;
        let artifacts = ArtifactsConfig::try_from_argv_then_env(cli.artifacts.clone().into_layer())?;
        let mirror = GithubMirrorConfig::try_from_argv_then_env(cli.github.clone().into_layer())?;
        let source = SourceConfig::try_from_argv_then_env(cli.source.clone().into_layer())?;
        let control_core_wasm = ControlCoreConfig::try_from_argv_then_env(cli.control_core.clone().into_layer())?.wasm;
        Ok(Self { rpc_port, http_port, store, artifacts, mirror, source, control_core_wasm })
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
    /// the artifacts record, the RPC server, and the HTTP ingress + REST control
    /// api (#3498) — so it lists those directly.
    /// The hub's binary store forks `<binary> --describe` once at upload time to
    /// capture this.
    #[must_use]
    pub fn describe_manifest() -> BinaryManifest {
        let caps = vec![
            <TraceDispatchCapability as Addressable>::NAMESPACE.to_owned(),
            <StoreCapability as Addressable>::NAMESPACE.to_owned(),
            <ArtifactsCapability as Addressable>::NAMESPACE.to_owned(),
            <MirrorDriverCapability as Addressable>::NAMESPACE.to_owned(),
            <SourceCapability as Addressable>::NAMESPACE.to_owned(),
            <ComponentHostCapability as Addressable>::NAMESPACE.to_owned(),
            <RpcServerCapability as Addressable>::NAMESPACE.to_owned(),
            <HttpServerCapability as Addressable>::NAMESPACE.to_owned(),
            <BloomeryApiCapability as Addressable>::NAMESPACE.to_owned(),
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
        let BloomeryEnv { rpc_port, http_port, store, artifacts, mirror, source, control_core_wasm } = env;
        let boot = SubstrateBoot::builder("aether-bloomery", env!("CARGO_PKG_VERSION")).build()?;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // A second handle for the post-build autoload drain: `mailer` is moved
        // into the builder, so the drain pushes through this clone.
        let autoload_mailer = Arc::clone(&boot.queue);
        let rpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);
        let http_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), http_port);
        // The component host lets the coordinator load the control-core wasm
        // actor (ADR-0149 §Packaging) — the reducer runtime that owns the live
        // snapshot. Built from the same wasmtime engine/linker/outbound the boot
        // set up, mirroring the headless chassis.
        let component_host = ComponentHostConfig {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };

        // The driver owns the boot and drops it on the shutdown signal.
        let driver = BloomeryDriverCapability { boot };

        let built = Builder::<Self>::new(registry, mailer)
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<StoreCapability>(store)
            .with_actor::<ArtifactsCapability>(artifacts)
            .with_actor::<MirrorDriverCapability>(mirror)
            .with_actor::<SourceCapability>(source)
            .with_actor::<ComponentHostCapability>(component_host)
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: rpc_addr.to_string(),
                peer_kind: PeerKind::Substrate {
                    engine_name: "aether-bloomery".into(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
            })
            // The REST control ingress (ADR-0149 §Packaging, #3498): the HTTP
            // server cap binds localhost, and the api cap claims the control
            // routes on it. RPC stays mounted above for fleet plumbing.
            .with_actor::<HttpServerCapability>(HttpServerConfig {
                enabled: true,
                bind_addr: http_addr.to_string(),
                ..HttpServerConfig::default()
            })
            .with_actor::<BloomeryApiCapability>(())
            .driver(driver)
            .build()?;

        // Autoload the control-core component if a wasm path is configured, so
        // the coordinator comes up as a running control loop with no external
        // load step. Fire-and-forward after `build` (the component-host mailbox
        // is claimed and the worker pool is up), mirroring the bundle chassis
        // autoload drain — the component is live shortly after `run` begins.
        if let Some(path) = control_core_wasm {
            let wasm = fs::read(&path).map_err(|error| BootError::Other(Box::new(error)))?;
            let payload = LoadComponent { wasm, name: None, config: Vec::new(), export: None }.encode_into_bytes();
            // Boot-time wire mail to the well-known component-host mailbox — a
            // reference id derivation, not sibling-cap addressing (the
            // `autoload_mail` precedent).
            #[allow(clippy::disallowed_methods)]
            let recipient = mailbox_id_from_name(<ComponentHostCapability as Addressable>::NAMESPACE);
            autoload_mailer.push(Mail::new(recipient, LoadComponent::ID, payload, 1));
        }
        Ok(built)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ArtifactsConfig, BloomeryChassis, BloomeryEnv, Chassis, GithubMirrorConfig};
    use crate::source::SourceConfig;
    use crate::store::StoreConfig;

    #[test]
    fn chassis_boots_and_claims_its_mailboxes() {
        // Port 0 → an OS-assigned ephemeral RPC port; the default `:memory:`
        // store touches no filesystem, and the artifacts store points at a temp
        // root so the test opens no data dir. The default (unconfigured) mirror
        // mounts the driver disabled — no timer, no network — so the chassis
        // boots clean without a token, and the default source config connects no
        // network (`ReqwestGithub::new` builds a client with no request). A
        // successful `build` boots every passive (store, artifacts, mirror,
        // source, trace, component host, rpc) and claims each mailbox — a claim
        // conflict or a failed store/shell open would surface as a `BootError`,
        // so `build` returning `Ok` is the assertion that the `aether.store`,
        // `aether.artifacts`, `aether.bloomery.mirror`, `aether.source`, and
        // `aether.component` mailboxes were claimed (the component host is the
        // reducer-actor load surface, ADR-0149 §Packaging).
        let artifacts_root = tempfile::tempdir().unwrap();
        let env = BloomeryEnv {
            rpc_port: 0,
            // Port 0 → an OS-assigned ephemeral HTTP port, so the REST ingress
            // (and its api cap) claim their mailboxes without a fixed-port clash.
            http_port: 0,
            store: StoreConfig::default(),
            artifacts: ArtifactsConfig { root: Some(artifacts_root.path().to_str().unwrap().to_owned()) },
            mirror: GithubMirrorConfig::default(),
            source: SourceConfig::default(),
            // No autoload: this test asserts the passive caps claim their
            // mailboxes; component autoload is exercised by the control_loop
            // integration test.
            control_core_wasm: None,
        };
        let chassis = BloomeryChassis::build(env).expect("bloomery chassis boots and claims its mailboxes");
        assert_eq!(BloomeryChassis::PROFILE, "bloomery");
        // Dropped without `run()` — teardown, no signal wait.
        drop(chassis);
    }
}
