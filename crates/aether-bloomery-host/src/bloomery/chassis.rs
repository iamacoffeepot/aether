//! `BloomeryChassis` — the coordinator chassis (ADR-0149 §Packaging).
//!
//! Assembled with the substrate builder like the hub, minus any render/audio/
//! window surface: `TraceDispatchCapability` (settlement + trace for local
//! dispatch), the `SQLite`-backed `StoreCapability`, `RpcServerCapability` (the
//! external typed-mail ingress the Demo dials), and a signal-blocking driver.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use aether_actor::Addressable;
use aether_capabilities::http::{HttpServerCapability, HttpServerConfig};
use aether_component::{ComponentHostCapability, ComponentHostConfig};
use aether_kinds::BinaryManifest;
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigError;
use aether_substrate::{Chassis, SubstrateBoot};
use aether_trace::TraceDispatchCapability;

use crate::api::{ApiConfig, BloomeryApiCapability};
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
            <ControlCore as Addressable>::NAMESPACE.to_owned(),
            <ArtifactsCapability as Addressable>::NAMESPACE.to_owned(),
            <MirrorReactorCapability as Addressable>::NAMESPACE.to_owned(),
            <ExecutorReactorCapability as Addressable>::NAMESPACE.to_owned(),
            <LandReactorCapability as Addressable>::NAMESPACE.to_owned(),
            <IntegrateReactorCapability as Addressable>::NAMESPACE.to_owned(),
            <SessionPoolCapability as Addressable>::NAMESPACE.to_owned(),
            <SourceCapability as Addressable>::NAMESPACE.to_owned(),
            <SigningCapability as Addressable>::NAMESPACE.to_owned(),
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
        let BloomeryEnv { rpc_port, http_port, store, artifacts, github, session, signing } = env;
        // Capture the tier-policy path before `github` is moved into the source
        // cap below; the api cap's pre-seal approve gate loads it at init (#3583).
        let approval_policy_file = github.approval_policy_file.clone();
        let boot = SubstrateBoot::builder("aether-bloomery", env!("CARGO_PKG_VERSION")).build()?;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        let rpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);
        let http_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), http_port);
        // The component host serves on-demand `aether.component.load` over RPC (the
        // MCP harness / fleet load components at runtime). Built from the same
        // wasmtime engine/linker/outbound the boot set up, mirroring the headless
        // chassis.
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
            // The single-writer control core (ADR-0149 §The control core): owns the
            // live snapshot, drives `reduce`, commits through the store, and gates
            // seals on the source claim refs. Native since the wasm-boundary
            // retirement — the api and reactors address it as a typed peer.
            .with_actor::<ControlCore>(())
            .with_actor::<ArtifactsCapability>(artifacts)
            .with_actor::<MirrorReactorCapability>(github.clone())
            // The executor dispatch reactor (#3505): drains the reducer's
            // dispatch-topic decisions, submits them through the
            // executor port, and admits matched results back to the control core.
            // Reuses the one GitHub-connection config the mirror + source caps do.
            .with_actor::<ExecutorReactorCapability>(github.clone())
            // The land reactor (#3559, ADR-0149 migration step 3): drains the
            // reducer's `aether.bloomery.land` decisions, issues the source-port
            // compare-and-swap that is now the landing of record, and admits
            // `Fact::Land` back to the control core. Reuses the one
            // GitHub-connection config the mirror + executor + source caps do.
            .with_actor::<LandReactorCapability>(github.clone())
            // The integrate reactor (#3650, ADR-0152): drains the reducer's
            // `aether.bloomery.integrate` decisions, folds the claimed candidate
            // onto the bloom's integration branch, and admits `Fact::Resolve`
            // back to the control core. Reuses the same GitHub-connection config.
            .with_actor::<IntegrateReactorCapability>(github.clone())
            // App-key custody (ADR-0149 §Migration step 3) is not a mounted
            // mailbox: the host-local minter (`app_auth::AppTokenSource`) is an
            // in-process `TokenSource` the port shells' client pulls from in
            // `connect_client`, reading the App key and failing fast there
            // (ADR-0150). This cap wires that same shared github config into the
            // source shell.
            .with_actor::<SourceCapability>(github)
            .with_actor::<SessionPoolCapability>(session)
            // The statement-signature custody point (ADR-0149 step 3): the
            // answer gate dials it to verify author signatures against the
            // host-local allowlist rather than the fake always-valid provider.
            .with_actor::<SigningCapability>(signing)
            .with_actor::<ComponentHostCapability>(component_host)
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: rpc_addr.to_string(),
                peer_kind: PeerKind::Substrate {
                    engine_name: "aether-bloomery".into(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
                // The bloomery host fields no engine-addressed forwards
                // (it wires no engines cap), so it needs no route target.
                route_target: None,
            })
            // The REST control ingress (ADR-0149 §Packaging, #3498): the HTTP
            // server cap binds localhost, and the api cap claims the control
            // routes on it. RPC stays mounted above for fleet plumbing.
            .with_actor::<HttpServerCapability>(HttpServerConfig {
                enabled: true,
                bind_addr: http_addr.to_string(),
                ..HttpServerConfig::default()
            })
            .with_actor::<BloomeryApiCapability>(ApiConfig { approval_policy_file })
            .driver(driver)
            .build()?;

        Ok(built)
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
