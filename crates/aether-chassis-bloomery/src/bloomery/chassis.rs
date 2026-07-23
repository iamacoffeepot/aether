//! `BloomeryChassis` — the coordinator chassis (ADR-0149 §Packaging).
//!
//! Assembled with the substrate builder like the hub, minus any render/audio/
//! window surface: `TraceDispatchCapability` (settlement + trace for local
//! dispatch), the `SQLite`-backed `StoreCapability`, `RpcServerCapability` (the
//! external typed-mail ingress the Demo dials), and a signal-blocking driver.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_http::{HttpServerCapability, HttpServerConfig};
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig, RpcServerParams};
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::{BootableChassis, BuildProvenance};
use aether_substrate::config::ConfigError;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use aether_substrate::{Chassis, SubstrateBoot};
use aether_trace::TraceDispatchCapability;

use crate::api::{ApiParams, BloomeryApiCapability};
use crate::artifacts::{ArtifactsCapability, ArtifactsConfig};
use crate::bloomery::cli::BloomeryCli;
use crate::bloomery::driver::BloomeryDriverCapability;
use crate::bloomery::mirror::GithubMirrorConfig;
use crate::bloomery::{
    ExecutorReactorCapability, IntegrateReactorCapability, LandReactorCapability, MirrorReactorCapability,
};
use crate::control::ControlCore;
use crate::session::{SessionConfig, SessionPoolCapability};
use crate::signing::{SigningCapability, SigningConfig};
use crate::source::SourceCapability;
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
    /// The shared GitHub connection configuration serving both the mirror reactor
    /// and the git source-port capability (one config, not two —
    /// `SourceConfig` is a re-export of `GithubMirrorConfig`). Unconfigured
    /// (empty token/owner/repo) mounts the mirror reactor disabled.
    pub github: GithubMirrorConfig,
    /// The executor session-reuse pool configuration.
    pub session: SessionConfig,
    /// The `aether.signing` capability's host-local authorized-signer allowlist
    /// (ADR-0149 step 3, ADR-0150). Unconfigured → no authorized signers, so the
    /// answer gate rejects every signature (fail-closed).
    pub signing: SigningConfig,
}

impl BloomeryEnv {
    /// Resolve every knob from `AETHER_*` env (and literal defaults), with no
    /// argv overlay — the env-only entry point. Delegates to
    /// [`Self::resolve`] with an empty [`BloomeryCli`], so the two
    /// paths resolve identically when argv is absent.
    ///
    /// # Errors
    ///
    /// See [`Self::resolve`].
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::resolve(&BloomeryCli::default())
    }

    /// ADR-0090 unit d: resolve every knob argv > `AETHER_*` env > default.
    /// `--rpc-port` shadows `AETHER_RPC_PORT` and `--store-path` shadows
    /// `AETHER_STORE_PATH`, each riding the derive-`Config` argv-then-env path
    /// (no naked env reads). Mirrors the hub's `HubEnv::resolve`; takes
    /// `&BloomeryCli` by reference so the bin keeps `cli` for its
    /// `--describe` branch.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known env value (or argv overlay value)
    /// fails its parser.
    pub fn resolve(cli: &BloomeryCli) -> Result<Self, ConfigError> {
        let rpc_port = RpcPortConfig::try_from_argv_then_env(cli.rpc.clone().into_layer())?.port;
        let http_port = HttpPortConfig::try_from_argv_then_env(cli.http.clone().into_layer())?.port;
        let store = StoreConfig::try_from_argv_then_env(cli.store.clone().into_layer())?;
        let artifacts = ArtifactsConfig::try_from_argv_then_env(cli.artifacts.clone().into_layer())?;
        let github = GithubMirrorConfig::try_from_argv_then_env(cli.github.clone().into_layer())?;
        let session = SessionConfig::try_from_argv_then_env(cli.session.clone().into_layer())?;
        let signing = SigningConfig::try_from_argv_then_env(cli.signing.clone().into_layer())?;
        Ok(Self { rpc_port, http_port, store, artifacts, github, session, signing })
    }
}

impl Chassis for BloomeryChassis {
    const PROFILE: &'static str = "bloomery";
    type Driver = BloomeryDriverCapability;
    type Env = BloomeryEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        let boot = SubstrateBoot::build()?;
        let builder = Self::compose(&boot, env);
        // The driver owns the boot and drops it on the shutdown signal — it
        // moves in here, after `compose` finished borrowing it.
        let driver = BloomeryDriverCapability { boot };
        builder.driver(driver).build()
    }
}

impl BloomeryChassis {
    /// This crate's `build.rs`-baked build provenance (ADR-0115): the source
    /// revision, build profile, and target triple, read back via `env!`, which
    /// resolves in *this* crate — the one whose `build.rs` set them.
    ///
    /// Bloomery deliberately does not depend on the `aether-chassis` aggregate,
    /// so it cannot reuse that crate's `build_provenance`. ADR-0162's prelude
    /// takes provenance as a value for exactly this reason: the bloomery binary
    /// fills a [`BuildProvenance`] here and hands it to the shared
    /// [`run_chassis_prelude`](aether_substrate::chassis::run_chassis_prelude),
    /// routing through the same `--describe` flow every chassis binary runs
    /// without forking it. `--describe` stops before Init, so it opens no
    /// `SQLite` store / artifacts dir and binds no socket. The hub's binary
    /// store forks `<binary> --describe` once at upload time to capture this.
    #[must_use]
    pub fn build_provenance() -> BuildProvenance {
        BuildProvenance {
            git_sha: env!("AETHER_GIT_SHA").to_owned(),
            profile: env!("AETHER_BUILD_PROFILE").to_owned(),
            target: env!("AETHER_TARGET_TRIPLE").to_owned(),
        }
    }
}

impl BootableChassis for BloomeryChassis {
    fn resolve_env() -> Result<Self::Env, ConfigError> {
        BloomeryEnv::from_env()
    }

    /// Compose the bloomery capability chain — the single claim/build path
    /// (ADR-0155) both [`Chassis::build`] and the shared describe prelude run,
    /// so the manifest roster can never drift from what boots. Returns the
    /// composed builder before the driver is installed: [`Chassis::build`] adds
    /// the signal-blocking driver and starts, while the prelude's claim ceremony
    /// (`describe_caps`) reads the claim terminal off it. Takes the boot handle
    /// by reference so [`Chassis::build`] can move the same `boot` into the
    /// driver afterward.
    fn compose(boot: &SubstrateBoot, env: BloomeryEnv) -> Builder<Self> {
        let BloomeryEnv { rpc_port, http_port, store, artifacts, github, session, signing } = env;
        // Capture the tier-policy path before `github` is moved into the source
        // cap below; the api cap's pre-seal approve gate loads it at init (#3583).
        let approval_policy_file = github.approval_policy_file.clone();
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        let http_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), http_port);
        // The component host serves on-demand `aether.component.load` over RPC (the
        // MCP harness / fleet load components at runtime). Built from the same
        // wasmtime engine/linker/outbound the boot set up, mirroring the headless
        // chassis.
        let component_host = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };

        Builder::<Self>::new(registry, mailer)
            // ADR-0063: production chassis configures the fatal-abort aborter so a
            // wasm guest trap exits the substrate via `lifecycle::fatal_abort`
            // instead of unwinding. Bloomery hosts wasm (`ComponentHostCapability`
            // below), so it needs the aborter the desktop/headless composes install.
            .with_aborter(Arc::new(OutboundFatalAborter::new(Arc::clone(&boot.outbound))))
            .with_actor::<TraceDispatchCapability>(())
            .with_actor_configured::<StoreCapability>((), store)
            // The single-writer control core (ADR-0149 §The control core): owns the
            // live snapshot, drives `reduce`, commits through the store, and gates
            // seals on the source claim refs. Native since the wasm-boundary
            // retirement — the api and reactors address it as a typed peer.
            .with_actor::<ControlCore>(())
            .with_actor_configured::<ArtifactsCapability>((), artifacts)
            .with_actor_configured::<MirrorReactorCapability>((), github.clone())
            // The executor dispatch reactor (#3505): drains the reducer's
            // dispatch-topic decisions, submits them through the
            // executor port, and admits matched results back to the control core.
            // Reuses the one GitHub-connection config the mirror + source caps do.
            .with_actor_configured::<ExecutorReactorCapability>((), github.clone())
            // The land reactor (#3559, ADR-0149 migration step 3): drains the
            // reducer's `aether.bloomery.land` decisions, issues the source-port
            // compare-and-swap that is now the landing of record, and admits
            // `Fact::Land` back to the control core. Reuses the one
            // GitHub-connection config the mirror + executor + source caps do.
            .with_actor_configured::<LandReactorCapability>((), github.clone())
            // The integrate reactor (#3650, ADR-0152): drains the reducer's
            // `aether.bloomery.integrate` decisions, folds the claimed candidate
            // onto the bloom's integration branch, and admits `Fact::Resolve`
            // back to the control core. Reuses the same GitHub-connection config.
            .with_actor_configured::<IntegrateReactorCapability>((), github.clone())
            // App-key custody (ADR-0149 §Migration step 3) is not a mounted
            // mailbox: the host-local minter (`app_auth::AppTokenSource`) is an
            // in-process `TokenSource` the port shells' client pulls from in
            // `connect_client`, reading the App key and failing fast there
            // (ADR-0150). This cap wires that same shared github config into the
            // source shell.
            .with_actor_configured::<SourceCapability>((), github)
            .with_actor_configured::<SessionPoolCapability>((), session)
            // The statement-signature custody point (ADR-0149 step 3): the
            // answer gate dials it to verify author signatures against the
            // host-local allowlist rather than the fake always-valid provider.
            .with_actor_configured::<SigningCapability>((), signing)
            .with_actor::<ComponentHostCapability>(component_host)
            .with_actor_configured::<RpcServerCapability>(
                RpcServerParams {
                    peer_kind: PeerKind::Substrate {
                        engine_name: aether_substrate::engine_name::<Self>(),
                        engine_version: env!("CARGO_PKG_VERSION").into(),
                        kinds: vec![],
                    },
                    // The bloomery host fields no engine-addressed forwards
                    // (it wires no engines cap), so it needs no route target.
                    route_target: None,
                },
                RpcServerConfig { port: Some(rpc_port) },
            )
            // The REST control ingress (ADR-0149 §Packaging, #3498): the HTTP
            // server cap binds localhost, and the api cap claims the control
            // routes on it. RPC stays mounted above for fleet plumbing.
            .with_actor_configured::<HttpServerCapability>(
                (),
                HttpServerConfig { enabled: true, bind_addr: http_addr.to_string(), ..HttpServerConfig::default() },
            )
            .with_actor::<BloomeryApiCapability>(ApiParams { approval_policy_file })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ArtifactsConfig, BloomeryChassis, BloomeryEnv, Chassis, GithubMirrorConfig, SessionConfig};
    use crate::signing::SigningConfig;
    use crate::store::StoreConfig;

    #[test]
    fn chassis_boots_and_claims_its_mailboxes() {
        // Port 0 → an OS-assigned ephemeral RPC port; the default `:memory:`
        // store touches no filesystem, and the artifacts store points at a temp
        // root so the test opens no data dir. The default (unconfigured) shared
        // GitHub config mounts the mirror reactor disabled — no timer, no network
        // — and connects no source network (`ReqwestGithub::new` builds a client
        // with no request); the default `:memory:` session pool touches no
        // filesystem. A successful `build` boots every passive (store, artifacts,
        // mirror, executor, land, source, session, trace, component host, rpc) and
        // claims each mailbox — a claim conflict or a failed store/shell open would
        // surface as a `BootError`, so `build` returning `Ok` is the assertion that
        // the `aether.store`, `aether.artifacts`, `aether.bloomery.mirror`,
        // `aether.bloomery.land`, `aether.session`, `aether.source`,
        // `aether.signing`, and `aether.component` mailboxes were claimed (the
        // land reactor mounts disabled under the default config; the component host is the
        // reducer-actor load surface, ADR-0149 §Packaging). App-key custody is
        // not a mounted mailbox — the shells' `connect_client` reads the key
        // in-process (ADR-0150), so the default (unconfigured) github config
        // reads no key and opens no network — and the signing cap's default
        // allowlist is empty, so its boot parses no keys.
        let artifacts_root = tempfile::tempdir().unwrap();
        let env = BloomeryEnv {
            rpc_port: 0,
            // Port 0 → an OS-assigned ephemeral HTTP port, so the REST ingress
            // (and its api cap) claim their mailboxes without a fixed-port clash.
            http_port: 0,
            store: StoreConfig::default(),
            artifacts: ArtifactsConfig { root: Some(artifacts_root.path().to_str().unwrap().to_owned()) },
            github: GithubMirrorConfig::default(),
            // The default `:memory:` pool touches no filesystem, so the session
            // cap claims `aether.session` without a data-dir open.
            session: SessionConfig::default(),
            // The default (unconfigured) allowlist mounts the signing cap with no
            // authorized signers — it claims `aether.signing` without parsing keys.
            signing: SigningConfig::default(),
        };
        let chassis = BloomeryChassis::build(env).expect("bloomery chassis boots and claims its mailboxes");
        assert_eq!(BloomeryChassis::PROFILE, "bloomery");
        // Dropped without `run()` — teardown, no signal wait.
        drop(chassis);
    }
}
