//! `BloomeryChassis` — the coordinator chassis (ADR-0149 §Packaging).
//!
//! Assembled with the substrate builder like the hub, minus any render/audio/
//! window surface: `TraceDispatchCapability` (settlement + trace for local
//! dispatch), the `SQLite`-backed `StoreCapability`, `RpcServerCapability` (the
//! external typed-mail ingress the Demo dials), and a signal-blocking driver.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

#[cfg(feature = "github")]
use aether_bloomery::SharedCorrespondence;
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_http::{HttpServerCapability, HttpServerConfig};
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig, RpcServerParams};
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::{BootableChassis, BuildProvenance, composed};
use aether_substrate::config::ConfigError;
use aether_substrate::{Chassis, SubstrateBoot};
use aether_trace::TraceDispatchCapability;

#[cfg(feature = "github")]
use super::local_landing::LocalLanding;
use crate::api::{ApiParams, BloomeryApiCapability};
use crate::artifacts::{ArtifactsCapability, ArtifactsConfig};
use crate::bloomery::CoordinatorConfig;
use crate::bloomery::cli::BloomeryCli;
use crate::bloomery::doctor::KitReport;
use crate::bloomery::driver::BloomeryDriverCapability;
#[cfg(feature = "github")]
use crate::bloomery::{
    CandidatePush, ClaimReleaseReactorCapability, ClaimReleaseReactorSetup, ExecutorReactorCapability,
    ExecutorReactorSetup, ExecutorShell, GithubConnectionConfig, IntegrateReactorCapability, IntegrateReactorSetup,
    JanitorReactorCapability, JanitorReactorSetup, LandReactorCapability, LandReactorSetup, LaneProgram,
    MirrorReactorCapability, MirrorReactorSetup, ProjectionShell, SourceReplicaShell, SourceShell, candidate_push_at,
    github_push_url,
};
use crate::control::{ControlCore, ControlSetup};
use crate::session::{SessionConfig, SessionPoolCapability};
use crate::signing::{SigningCapability, SigningConfig};
#[cfg(feature = "github")]
use crate::source::SourceCapability;
#[cfg(feature = "github")]
use crate::source::SourceSetup;
#[cfg(feature = "github")]
use crate::store::SqliteCorrespondence;
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

#[cfg(feature = "github")]
fn mounted_correspondence<T>(
    mounted: Option<&T>,
    correspondence: &SharedCorrespondence,
) -> Option<SharedCorrespondence> {
    mounted.map(|_| Arc::clone(correspondence))
}

#[cfg(feature = "github")]
struct BloomeryActorSetups {
    mirror: MirrorReactorSetup,
    executor: ExecutorReactorSetup,
    land: LandReactorSetup,
    integrate: IntegrateReactorSetup,
    claim_release: ClaimReleaseReactorSetup,
    janitor: JanitorReactorSetup,
    source: SourceSetup,
    correspondence: SharedCorrespondence,
    pusher: Arc<dyn CandidatePush>,
}

#[cfg(feature = "github")]
fn correspondence_store(
    github_connection: &GithubConnectionConfig,
    store_path: &str,
) -> Result<SharedCorrespondence, BootError> {
    #[cfg(not(any(test, feature = "testing")))]
    let _ = github_connection;

    #[cfg(any(test, feature = "testing"))]
    if github_connection.uses_fixture() {
        return Ok(Arc::new(github_connection.shared_fixture()));
    }

    Ok(Arc::new(SqliteCorrespondence::open(store_path).map_err(|error| BootError::Other(Box::new(error)))?))
}

#[cfg(feature = "github")]
fn source_shell(
    github: &GithubConnectionConfig,
    coordinator: &CoordinatorConfig,
    correspondence: SharedCorrespondence,
) -> Result<SourceShell, BootError> {
    #[cfg(any(test, feature = "testing"))]
    if github.uses_fixture() {
        return Ok(github.fixture_source(coordinator.mainline(), correspondence));
    }

    if coordinator.uses_local_authority() {
        return SourceShell::connect_local(
            &coordinator.authority_repo,
            coordinator,
            correspondence,
            github.cas_land_enabled,
        )
        .map_err(|error| BootError::Other(Box::new(error)));
    }

    SourceShell::connect(github, coordinator, correspondence).map_err(|error| BootError::Other(Box::new(error)))
}

#[cfg(feature = "github")]
fn landing_source(
    github: &GithubConnectionConfig,
    coordinator: &CoordinatorConfig,
    correspondence: SharedCorrespondence,
) -> Result<Arc<dyn aether_bloomery_github::LandingSource>, BootError> {
    #[cfg(any(test, feature = "testing"))]
    if github.uses_fixture() {
        return Ok(github.fixture_landing(coordinator.mainline(), correspondence));
    }

    let client = github.connect_client().map_err(|error| BootError::Other(Box::new(error)))?;
    let source =
        aether_bloomery_github::GitSource::new(client, correspondence, github.cas_land_enabled, coordinator.mainline());
    Ok(Arc::new(aether_bloomery_github::GithubLanding::new(source)))
}

#[cfg(feature = "github")]
fn projection_shell(github: &GithubConnectionConfig, configured: bool) -> Result<Option<ProjectionShell>, BootError> {
    #[cfg(any(test, feature = "testing"))]
    if github.uses_fixture() {
        return Ok(Some(ProjectionShell::new(Arc::new(aether_bloomery_github::GithubProjection::new(
            github.shared_fixture(),
        )))));
    }

    configured.then(|| ProjectionShell::connect(github)).transpose().map_err(|error| BootError::Other(Box::new(error)))
}

#[cfg(feature = "github")]
fn actor_setups(
    github: &GithubConnectionConfig,
    coordinator: &CoordinatorConfig,
    session: &SessionConfig,
) -> Result<BloomeryActorSetups, BootError> {
    let configured = github.uses_fixture() || github.missing_connection_knobs().is_empty();
    let source_configured = configured || coordinator.uses_local_authority();
    let replica_enabled = coordinator.source_replica_enabled(github);
    if replica_enabled {
        coordinator.require_single_writer_marker(github).map_err(|error| BootError::Other(Box::new(error)))?;
    }
    let repository = configured.then(|| (github.owner.clone(), github.repo.clone()));
    let correspondence = correspondence_store(github, &coordinator.store_path)?;
    let source = source_shell(github, coordinator, Arc::clone(&correspondence))?;
    let executor = (!(!configured && !coordinator.local_lane_enabled))
        .then(|| ExecutorShell::connect(github, coordinator, Arc::clone(&correspondence), session))
        .transpose()
        .map_err(|error| BootError::Other(Box::new(error)))?;
    let executor_correspondence = mounted_correspondence(executor.as_ref(), &correspondence);
    let repo = coordinator.lane_repository();
    // A testing-featured binary must never `git push` to `origin` — cargo test
    // forks exactly that binary with its cwd inside the live checkout (#4842).
    // A local authority's remote is an absolute path, never origin, so the
    // hermetic publication path is the one this refuse exists to protect.
    let refuse_origin =
        (cfg!(any(test, feature = "testing")) || github.uses_fixture()) && !coordinator.uses_local_authority();
    let pusher = candidate_push_at(refuse_origin, repo.clone(), coordinator.candidate_remote());

    Ok(BloomeryActorSetups {
        mirror: MirrorReactorSetup {
            projection: projection_shell(github, configured)?,
            source: configured.then(|| source.clone()),
            replica: replica_enabled.then(|| {
                SourceReplicaShell::connect(
                    &coordinator.authority_repo,
                    &github_push_url(&github.api_base, &github.owner, &github.repo),
                    coordinator.mainline(),
                    &github.token,
                )
            }),
            poll_interval_secs: coordinator.poll_interval_secs,
            repository: repository.clone(),
        },
        executor: ExecutorReactorSetup {
            executor: executor.clone(),
            correspondence: executor_correspondence,
            store_path: coordinator.store_path.clone(),
            artifacts_root: coordinator.artifacts_root.clone(),
            poll_interval_secs: coordinator.poll_interval_secs,
            stale_warn_after_secs: coordinator.stale_warn_after_secs,
            heartbeat_silence_secs: coordinator.heartbeat_silence_secs()?,
            repository: repository.clone(),
            disabled_missing: github.missing_connection_knobs(),
            // Build shape first, configuration second (#4842). A `testing`-featured
            // binary must never reach a real `git push` whatever backend it names —
            // `cargo test` forks exactly such a binary, and five cross-process tests
            // boot it with no backend env and a cwd inside the real checkout, whose
            // `origin` is the live repository. Keying on `uses_fixture()` alone left
            // the guarantee resting on those scenarios happening not to produce an
            // admitted capture. The `testing` feature is dev-only (no shipping
            // manifest enables it), so this cannot refuse a real deployment's push.
            pusher: Arc::clone(&pusher),
        },
        land: LandReactorSetup {
            source: if coordinator.uses_local_authority() {
                Some(Arc::new(LocalLanding::new(source.clone())))
            } else {
                configured.then(|| landing_source(github, coordinator, Arc::clone(&correspondence))).transpose()?
            },
            store_path: coordinator.store_path.clone(),
            poll_interval_secs: coordinator.poll_interval_secs,
            repository: repository.clone(),
            cas_land_enabled: github.cas_land_enabled,
            emit_source_replica: replica_enabled,
        },
        integrate: IntegrateReactorSetup {
            source: source_configured.then(|| source.clone()),
            store_path: coordinator.store_path.clone(),
            artifacts_root: coordinator.artifacts_root.clone(),
            poll_interval_secs: coordinator.poll_interval_secs,
            repository,
        },
        claim_release: ClaimReleaseReactorSetup {
            source: source_configured.then(|| source.clone()),
            store_path: coordinator.store_path.clone(),
            poll_interval_secs: coordinator.poll_interval_secs,
        },
        janitor: JanitorReactorSetup {
            source: source_configured.then(|| source.clone()),
            executor: executor.clone(),
            store_path: coordinator.store_path.clone(),
            worktree_base: coordinator.local_worktree_base.clone(),
            target_base: coordinator.lane_target_base.clone(),
            lane_target_budget_bytes: coordinator.lane_target_budget_bytes,
            evidence_retention_days: coordinator.evidence_retention_days,
            poll_interval_secs: coordinator.poll_interval_secs,
            repo: repo.display().to_string(),
        },
        source: SourceSetup { shell: source, claims_enabled: source_configured, mainline: coordinator.mainline() },
        correspondence,
        pusher,
    })
}

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
    /// The GitHub adapter connection, Actions, App-auth, and fixture settings.
    /// Unconfigured (empty token/owner/repo) mounts remote reactors disabled.
    #[cfg(feature = "github")]
    pub github: GithubConnectionConfig,
    /// Backend-neutral Bloomery coordinator settings.
    pub coordinator: CoordinatorConfig,
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
    /// (no naked env reads). Like the other chassis it resolves off the source
    /// stack; takes `&BloomeryCli` by reference so the bin keeps `cli` for its
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
        #[cfg(feature = "github")]
        let github = GithubConnectionConfig::try_from_argv_then_env(cli.github.clone().into_layer())?;
        let coordinator = CoordinatorConfig::try_from_argv_then_env(cli.coordinator.clone().into_layer())?;
        let session = SessionConfig::try_from_argv_then_env(cli.session.clone().into_layer())?;
        let signing = SigningConfig::try_from_argv_then_env(cli.signing.clone().into_layer())?;
        Ok(Self {
            rpc_port,
            http_port,
            store,
            artifacts,
            #[cfg(feature = "github")]
            github,
            coordinator,
            session,
            signing,
        })
    }
}

impl Chassis for BloomeryChassis {
    const PROFILE: &'static str = "bloomery";
    type Driver = BloomeryDriverCapability;
    type Env = BloomeryEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        let mut boot = SubstrateBoot::build()?;
        // Bloomery's base is the unit no-op — it stages no config sources — but it
        // still routes through `composed`, so it gets the framework-minted
        // `OutboundFatalAborter` by construction (previously the implicit
        // `PanicAborter`).
        let builder = composed::<Self>(&mut boot, (), env)?;
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
    type Base = ();

    fn resolve_env() -> Result<(Self::Base, Self::Env), ConfigError> {
        Ok(((), BloomeryEnv::from_env()?))
    }

    /// Compose the bloomery capability delta on top of the framework-minted,
    /// based builder [`composed`] hands it — the single claim/build path
    /// (ADR-0155) both [`Chassis::build`] and the shared describe prelude run,
    /// so the manifest roster can never drift from what boots. Bloomery's base is
    /// the unit no-op (it stages no config sources), so it keeps
    /// `TraceDispatchCapability` in this delta; the aborter is supplied by
    /// `composed`. Takes the boot handle by reference so [`Chassis::build`] can
    /// move the same `boot` into the driver afterward.
    #[cfg(feature = "github")]
    fn compose(builder: Builder<Self>, boot: &SubstrateBoot, env: BloomeryEnv) -> Result<Builder<Self>, BootError> {
        // The production lane program is the one that inherits the kit. A
        // mock-lane coordinator must not probe it here: each `--version` is a
        // process, and a loaded lane-boundary suite boots many coordinators at
        // once — a parked probe used to leave the RPC port unbound past the
        // handshake deadline (#5035).
        if LaneProgram::parse(&env.coordinator.local_lane_program) == LaneProgram::default() {
            KitReport::inspect().log_at_boot();
        }
        let BloomeryEnv { rpc_port, http_port, store, artifacts, github, coordinator, session, signing } = env;
        // Capture the tier-policy path before `github` is moved into the source
        // cap below; the api cap's pre-seal approve gate loads it at init (#3583).
        let approval_policy_file = coordinator.approval_policy_file.clone();
        let worktree_base = coordinator.local_worktree_base.clone();
        let artifacts_root = coordinator.artifacts_root.clone();
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
        let setups = actor_setups(&github, &coordinator, &session)?;

        // #3947's explicit `with_aborter` is superseded by the seam inversion:
        // `composed` (which `build` routes through) installs `OutboundFatalAborter`
        // on every chassis, so bloomery gets the aborter by construction. The
        // aborter behavior #3947 guards (control-core boot replay fatal-aborting on
        // a bad journal record) is preserved — see `tests/recovery.rs`.
        Ok(builder
            .with_actor::<TraceDispatchCapability>(())
            .with_actor_configured::<StoreCapability>((), store)
            // The single-writer control core (ADR-0149 §The control core): owns the
            // live snapshot, drives `reduce`, commits through the store, and gates
            // seals on the source claim refs. Native since the wasm-boundary
            // retirement — the api and reactors address it as a typed peer.
            .with_actor::<ControlCore>(ControlSetup {
                poll_interval_secs: coordinator.poll_interval_secs,
                artifacts_root: coordinator.artifacts_root,
            })
            .with_actor_configured::<ArtifactsCapability>((), artifacts)
            .with_actor::<MirrorReactorCapability>(setups.mirror)
            // The executor dispatch reactor (#3505): drains the reducer's
            // dispatch-topic decisions, submits them through the
            // executor port, and admits matched results back to the control core.
            // Receives only its assembled executor, correspondence view, and
            // backend-neutral coordinator scalars.
            .with_actor::<ExecutorReactorCapability>(setups.executor)
            // The land reactor (#3559, ADR-0149 migration step 3): drains the
            // reducer's `aether.bloomery.land` decisions, issues the source-port
            // compare-and-swap that is now the landing of record, and admits
            // `Fact::Land` back to the control core. Receives the already-built
            // source shell and its coordinator scalars.
            .with_actor::<LandReactorCapability>(setups.land)
            .with_actor::<ClaimReleaseReactorCapability>(setups.claim_release)
            // The janitor: reconciles the journal against on-disk worktrees,
            // evidence dirs, slot target dirs, and a terminal bloom's working
            // refs, so a kill or crash does not wait for the next boot to
            // reclaim what the happy-path release missed.
            .with_actor::<JanitorReactorCapability>(setups.janitor)
            // The integrate reactor (#3650, ADR-0152): drains the reducer's
            // `aether.bloomery.integrate` decisions, folds the claimed candidate
            // onto the bloom's integration branch, and admits `Fact::Resolve`
            // back to the control core. Receives the shared source shell clone
            // and its coordinator scalars.
            .with_actor::<IntegrateReactorCapability>(setups.integrate)
            // App-key custody (ADR-0149 §Migration step 3) is not a mounted
            // mailbox: the adapter's minter (`aether_bloomery_github::AppTokenSource`)
            // is an in-process `TokenSource` the port shells' client pulls from
            // in `connect_client`, which reads the host-local App key and fails
            // fast there (ADR-0150). The source actor receives only the
            // chassis-built shell and the claim-registry enable decision.
            .with_actor::<SourceCapability>(setups.source)
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
            .with_actor::<BloomeryApiCapability>(ApiParams {
                approval_policy_file,
                correspondence: Some(setups.correspondence),
                pusher: Some(setups.pusher),
                worktree_base,
                artifacts_root,
                control_token: coordinator.http_control_token,
            }))
    }
    #[cfg(not(feature = "github"))]
    fn compose(builder: Builder<Self>, boot: &SubstrateBoot, env: BloomeryEnv) -> Result<Builder<Self>, BootError> {
        KitReport::inspect().log_at_boot();
        let BloomeryEnv { rpc_port, http_port, store, artifacts, coordinator, session, signing } = env;
        let approval_policy_file = coordinator.approval_policy_file.clone();
        let worktree_base = coordinator.local_worktree_base.clone();
        let artifacts_root = coordinator.artifacts_root.clone();
        let http_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), http_port);
        let component_host = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };

        Ok(builder
            .with_actor::<TraceDispatchCapability>(())
            .with_actor_configured::<StoreCapability>((), store)
            .with_actor::<ControlCore>(ControlSetup {
                poll_interval_secs: coordinator.poll_interval_secs,
                artifacts_root: coordinator.artifacts_root,
            })
            .with_actor_configured::<ArtifactsCapability>((), artifacts)
            .with_actor_configured::<SessionPoolCapability>((), session)
            .with_actor_configured::<SigningCapability>((), signing)
            .with_actor::<ComponentHostCapability>(component_host)
            .with_actor_configured::<RpcServerCapability>(
                RpcServerParams {
                    peer_kind: PeerKind::Substrate {
                        engine_name: aether_substrate::engine_name::<Self>(),
                        engine_version: env!("CARGO_PKG_VERSION").into(),
                        kinds: vec![],
                    },
                    route_target: None,
                },
                RpcServerConfig { port: Some(rpc_port) },
            )
            .with_actor_configured::<HttpServerCapability>(
                (),
                HttpServerConfig { enabled: true, bind_addr: http_addr.to_string(), ..HttpServerConfig::default() },
            )
            .with_actor::<BloomeryApiCapability>(ApiParams {
                approval_policy_file,
                worktree_base,
                artifacts_root,
                control_token: coordinator.http_control_token,
            }))
    }
}

#[cfg(all(test, feature = "github"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use aether_bloomery::SharedCorrespondence;
    use aether_bloomery_github::testing::FakeGithub;

    use super::{
        ArtifactsConfig, BloomeryChassis, BloomeryEnv, Chassis, CoordinatorConfig, GithubConnectionConfig,
        SessionConfig, actor_setups, mounted_correspondence,
    };
    use crate::signing::SigningConfig;
    use crate::store::StoreConfig;

    // Tripwire: the boot site selects the push seam by **build shape**, not by
    // backend configuration (#4842). The configs here are the ones `cargo test`
    // actually forks with — `GithubConnectionConfig::default()` names no backend,
    // so `uses_fixture()` is false — which is exactly the case the previous
    // `default_candidate_push(github.uses_fixture())` resolved to a live
    // `GitCandidatePush`, in a process whose cwd is the real checkout and whose
    // `origin` is the live repository.
    //
    // So this fails if anyone drops the `cfg!` term, and it cannot pass for the
    // wrong reason: with `uses_fixture()` false, the build-shape term is the only
    // thing that can produce a refusal. What makes the regression worth a
    // tripwire rather than a comment is the consequence — an errant push here
    // carries an all-zero source sha, which git reads as a ref *deletion* that
    // exits 0 and logs as a successful capture (#4841).
    #[test]
    fn the_boot_seam_refuses_a_push_in_a_testing_build_that_names_no_fixture() {
        let github = GithubConnectionConfig::default();
        assert!(
            !github.uses_fixture(),
            "the default config names no fixture backend; configuration alone would not refuse"
        );

        let setups = actor_setups(&github, &CoordinatorConfig::default(), &SessionConfig::default())
            .expect("actor setups resolve under defaults");
        let refusal = setups
            .executor
            .pusher
            .push(&"0".repeat(40), "refs/heads/bloom/x/candidate/wp")
            .expect_err("a testing build's boot seam declines to push");

        assert!(refusal.contains("refusing to push"), "the boot seam resolved the refusing arm: {refusal}");
    }

    #[test]
    fn every_mounted_executor_retains_the_shared_correspondence() {
        let correspondence: SharedCorrespondence = Arc::new(FakeGithub::new());

        assert!(mounted_correspondence(Some(&()), &correspondence).is_some());
        assert!(mounted_correspondence::<()>(None, &correspondence).is_none());
    }

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
            github: GithubConnectionConfig::default(),
            coordinator: CoordinatorConfig::default(),
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
