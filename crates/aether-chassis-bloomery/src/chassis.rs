//! Bloomery chassis: [`BloomeryChassis`] (issue #6244), the journal-driven
//! engine. Boots the shared base stratum plus the component host and a held
//! RPC server, then the mount seam spawns the journal owner, the bundle
//! driver, and the inspect actor over the one configured unit's journal root,
//! and only then does the RPC listener bind (issue #6399), so an engine a
//! caller can reach can already take driver calls and inspect reads.
//!
//! The composition is deliberately narrow. Its integrations are HTTP egress
//! for Sampled programs (ADR-0234 decision 7), composed with the capability's
//! own deny-by-default allowlist, so a fetch reaches only the hosts an
//! operator names with `--http-allowlist` / `AETHER_HTTP_ALLOWLIST` and every
//! other fetch is answered with a refusal, and the `aether.bloomery.workspace` actor
//! (ADR-0237 decision 8), which runs steps and imports digest-pinned images
//! through the Docker Engine API at `--workspace-endpoint` /
//! `AETHER_WORKSPACE_ENDPOINT`, reading and staging through the journal each
//! request names as its `source` (ADR-0240 D7). A credential rides the HTTP
//! capability: `--http-secrets` binds a secret from the `--secrets-dir`
//! directory to an allowlisted host (ADR-0235), so no program carries one. The workspace
//! actor is the engine's only route to a container; `aether.process` is not
//! composed, and no TCP, HTTP-serving, or fs capability rides this engine,
//! while the RPC server and the base stratum's inventory keep it drivable
//! over MCP (ADR-0155 §3).

use std::io;
use std::mem;
use std::num::NonZeroUsize;
use std::sync::Arc;

use aether_bloomery_journal::{Journal, ReadCacheBudget, SystemClock};
use aether_bloomery_workspace::WorkspaceCapability;
use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, RegistryQueueConfig, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
    chassis_residual_knobs, install_frame_size, with_rpc_server,
};
use aether_chassis::cli::ChassisCli;
use aether_chassis::entry::ChassisEnv;
use aether_chassis::signal_driver::SignalDriverCapability;
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_http::HttpCapability;
use aether_rpc::RpcBindGate;
use aether_substrate::chassis::BootableChassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::composed;
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::{ConfigError, KnobRecord, validate_env};
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::cli::BloomeryCli;
use crate::config::{self, BloomeryConfig};
use crate::mount::{self, Mounted};

/// Marker type for the bloomery chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<BloomeryChassis>`] returned
/// by `Self::build`. Same shape as the other chassis markers post
/// ADR-0071 phase 3.
pub struct BloomeryChassis;

impl Chassis for BloomeryChassis {
    const PROFILE: &'static str = "bloomery";
    type Driver = SignalDriverCapability<Self>;
    type Env = BloomeryEnv;

    /// Build the bloomery chassis through [`BloomeryChassis::build_mounted`],
    /// dropping the mounted references: the shipped binary drives the engine
    /// over RPC and never addresses the journal or the driver in process.
    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_mounted(env).map(|(built, _)| built)
    }
}

impl BloomeryChassis {
    /// Build the bloomery chassis: the hub's prologue with headless's lift —
    /// lower the bloomery knobs, open the unit's journal root, stand up the
    /// substrate, re-apply the resolved log filter, lift the base out of the
    /// env, compose the shared stratum plus the component host, HTTP egress,
    /// the workspace actor, and the held RPC server, sweep for unknown env
    /// keys, install the signal-blocking driver, mount the journal owner, the
    /// bundle driver, and the inspect actor, and only then open the RPC
    /// server's bind gate. The order is build, mount, bind: until the gate
    /// opens a dial is refused, so a caller that reaches the engine can
    /// address every mounted actor
    /// (issue #6399). A chassis composed with no RPC port publishes no gate
    /// and binds nothing. The [`Mounted`] references come back beside the
    /// chassis for an embedder that drives it in process.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when the bloomery knobs do not lower, the
    /// journal root does not open (another engine holds it, among others),
    /// the substrate or the composed chain fails to boot, a mount spawn
    /// fails, or the RPC port cannot be bound.
    pub fn build_mounted(mut env: BloomeryEnv) -> Result<(BuiltChassis<Self>, Mounted), BootError> {
        // Lower the bloomery knobs first, before anything with a side effect:
        // a bad unit list or an out-of-range closure limit is a typo in the
        // operator's own argv, and refusing it here costs nothing, where
        // refusing it at the mount seam would first stand up wasmtime.
        // `--describe` / `--print-config` exit in `run_chassis_main`'s prelude
        // before `build` is called, so they never reach this and still answer
        // with no units configured.
        let bloomery = mem::take(&mut env.bloomery);
        let (units, limit) = bloomery.to_units_and_limit()?;
        let tick = bloomery.clock_tick()?;
        let invocations = bloomery.bundle_invocations()?;
        // `read_cache_bytes` is the engine total, split equally among the
        // units (ADR-0240 I-6). The share is taken from the whole lowered list;
        // an empty list is refused by `sole_unit` on the next line, so its
        // one-unit fallback never mounts a journal.
        let read_cache = ReadCacheBudget::share(
            bloomery.read_cache_bytes,
            NonZeroUsize::new(units.len()).unwrap_or(NonZeroUsize::MIN),
        );
        let unit = config::sole_unit(units)?;
        // Open the root before wasmtime too: a root another engine holds, or
        // one that cannot be created, refuses boot here, naming the root. The
        // journal and the driver read one clock (ADR-0245).
        let clock = Arc::new(SystemClock);
        let journal = Journal::open_with_clock(&unit.root, Box::new(Arc::clone(&clock))).map_err(|error| {
            BootError::Other(Box::new(io::Error::other(format!(
                "the bloomery journal root {} of unit `{}` does not open: {error}",
                unit.root.display(),
                unit.key
            ))))
        })?;
        let mut boot = SubstrateBoot::build()?;
        apply_filter(&env.runtime.log_filter);
        let base = mem::take(&mut env.base);
        let builder = composed::<Self>(&mut boot, base, env)?;
        validate_env(&builder.config_manifest().known_keys(&chassis_residual_knobs()))?;
        let built = builder.driver(SignalDriverCapability::new(boot)).build()?;
        let driver = mount::DriverSetup { limit, clock, tick, invocations };
        let mounted = mount::mount(&built, &unit.key, journal, read_cache, driver)?;
        tracing::info!(
            unit = %unit.key,
            journal_root = %unit.root.display(),
            journal = ?mounted.journal,
            driver = ?mounted.driver,
            inspect = ?mounted.inspect,
            "bloomery chassis mounted the journal owner, the bundle driver, and the inspect actor",
        );
        if let Some(gate) = built.handle::<RpcBindGate>() {
            gate.open().map_err(|error| BootError::Other(Box::new(error)))?;
        }
        Ok((built, mounted))
    }
}

/// The bloomery chassis env: the universal base stratum plus the substrate
/// runtime knobs and the bloomery's own knobs. Narrower than [`CommonEnv`](aether_chassis::boot::CommonEnv)
/// on purpose — the full-stack surface (fs roots, boot manifest, package
/// depot) belongs to engines that honour it, and here it would resolve
/// but never compose.
pub struct BloomeryEnv {
    /// The universal base stratum (config source stack + the non-cap ring /
    /// scheduler / settlement members) `composed` installs ahead of `compose`.
    pub base: ChassisBase,
    /// The substrate runtime knobs. Only `log_filter` is consumed
    /// chassis-side (re-applied after the subscriber installs, in
    /// [`BloomeryChassis::build_mounted`]); the field carries the whole resolved
    /// member so its values resolve once.
    pub runtime: RuntimeConfig,
    /// The bloomery knobs. Lowered to the unit list and the driver's
    /// closure limit at the top of [`BloomeryChassis::build_mounted`], then applied off
    /// the builder at the mount seam.
    pub bloomery: BloomeryConfig,
}

impl BloomeryEnv {
    /// An env over `base`, `runtime`, and `bloomery`.
    #[must_use]
    pub fn new(base: ChassisBase, runtime: RuntimeConfig, bloomery: BloomeryConfig) -> Self {
        Self { base, runtime, bloomery }
    }
}

impl ChassisEnv for BloomeryEnv {
    /// Open the bloomery env off the source stack the CLI root assembles:
    /// resolve the four `ChassisBase` members plus the runtime and bloomery
    /// members, then push the wire-frame cap into the codec.
    fn from_cli(cli: impl ChassisCli) -> Result<Self, ConfigError> {
        let mut sources = cli.into_sources()?;
        let actor_ring = sources.resolve::<ActorRingConfig>()?;
        let scheduler_tuning = sources.resolve::<SchedulerTuningConfig>()?;
        let registry_queues = sources.resolve::<RegistryQueueConfig>()?;
        let settlement = sources.resolve::<SettlementConfig>()?;
        let runtime = sources.resolve::<RuntimeConfig>()?;
        let bloomery = sources.resolve::<BloomeryConfig>()?;
        install_frame_size(&mut sources)?;
        Ok(Self::new(
            ChassisBase { sources, actor_ring, scheduler_tuning, registry_queues, settlement },
            runtime,
            bloomery,
        ))
    }
}

impl BootableChassis for BloomeryChassis {
    type Base = ChassisBase;

    /// Resolve the bloomery env off the default CLI root (no argv): the
    /// describe / config helpers hand `composed` the same base + env a real
    /// boot takes, so the manifests reflect the same chain that boots.
    fn resolve_env() -> Result<(Self::Base, Self::Env), ConfigError> {
        let mut env = BloomeryEnv::from_cli(BloomeryCli::default())?;
        let base = mem::take(&mut env.base);
        Ok((base, env))
    }

    fn residual_knobs() -> Vec<KnobRecord> {
        chassis_residual_knobs()
    }

    /// Compose the bloomery capability delta — the single claim/build path
    /// (ADR-0155) both [`Chassis::build`] and the describe / config helpers run,
    /// so the manifest roster can never drift from what boots. Adds only the
    /// component host (which the driver's `Command::Load` targets), HTTP
    /// egress for Sampled programs (ADR-0234 decision 7), the workspace actor
    /// (ADR-0237 decision 8), which holds no store and reads and stages
    /// through each request's source (ADR-0240 D7), the RPC server
    /// (ADR-0155 §3) composed held so [`BloomeryChassis::build_mounted`] binds
    /// it after the mount, and the bloomery config declaration. The inventory
    /// MCP resolves addresses and kinds through arrives from [`ChassisBase`].
    ///
    /// HTTP resolves `HttpConfig` off the source stack with no chassis-side
    /// override, so its compiled defaults hold: an empty allowlist answers
    /// every fetch `AllowlistDenied` before any connection. Composing it
    /// unconditionally is what makes a program's fetch always answered on the
    /// one engine that runs programs. `aether.process`, TCP, HTTP-serving,
    /// and fs capabilities are not composed.
    fn compose(builder: Builder<Self>, boot: &SubstrateBoot, _env: Self::Env) -> Result<Builder<Self>, BootError> {
        let component_host_params = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        Ok(with_rpc_server(
            builder
                .with_actor::<ComponentHostCapability>(component_host_params)
                .with_actor::<HttpCapability>(())
                .with_actor::<WorkspaceCapability>(()),
        )
        .declare_config_member::<BloomeryConfig>())
    }
}

#[cfg(test)]
mod config_manifest_tests {
    use super::BloomeryChassis;
    use aether_chassis::boot::chassis_residual_knobs;
    use aether_substrate::chassis::config_manifest;

    #[test]
    fn bloomery_known_keys_claim_its_knobs_http_egress_and_the_workspace() {
        // The aggregate is derived from what the bloomery actually composes: it
        // must claim its own two knobs, the RPC port, the HTTP egress knobs, and
        // the workspace endpoint, and must not claim any process/serving/fs
        // knob. Catches a dropped HTTP compose, which would bring back the
        // unanswered-fetch hang and make an operator's allowlist key an
        // unknown-env boot error; a dropped workspace compose, which would make
        // the endpoint key an unknown-env boot error and leave imports
        // unanswered; a later edit that composes `with_full_stack_caps`, which
        // would add process exec and fs; and a dropped `declare_config_member`
        // that would make the units knob warn as unknown. The retired
        // single-journal knob must stay unclaimed, so a stale env key is an
        // unknown-env boot error rather than a silently ignored journal.
        let manifest = config_manifest::<BloomeryChassis>().expect("bloomery config manifest");
        let known = manifest.known_keys(&chassis_residual_knobs());
        assert!(known.contains("AETHER_BLOOMERY_UNITS"), "bloomery must claim its units knob");
        assert!(!known.contains("AETHER_BLOOMERY_JOURNAL"), "the single-journal knob is retired");
        assert!(known.contains("AETHER_BLOOMERY_CLOSURE_LIMIT_BYTES"), "bloomery must claim its closure-limit knob");
        assert!(known.contains("AETHER_BLOOMERY_BUNDLE_INVOCATIONS"), "bloomery must claim its invocation-limit knob");
        assert!(known.contains("AETHER_RPC_PORT"), "bloomery must claim the RPC port via the composed RpcServerConfig");
        assert!(known.contains("AETHER_HTTP_ALLOWLIST"), "bloomery must claim the http egress allowlist knob");
        assert!(known.contains("AETHER_HTTP_DISABLE"), "bloomery must claim the http egress disable knob");
        assert!(known.contains("AETHER_WORKSPACE_ENDPOINT"), "bloomery must claim the workspace endpoint knob");
        assert!(
            !known.contains("AETHER_PROCESS_ALLOWLIST"),
            "bloomery composes the workspace, never aether.process, so it must not claim the process knob"
        );
        assert!(
            !known.contains("AETHER_HTTP_SERVER_ENABLED"),
            "bloomery composes no http server, so it must not claim the http-server knob"
        );
        assert!(!known.contains("AETHER_SAVE_DIR"), "bloomery composes no fs roots, so it must not claim the fs knob");
        assert!(
            !known.contains("AETHER_BOOT_MANIFEST"),
            "bloomery honours no boot manifest, so it must not claim the boot-manifest knob"
        );
    }
}
