//! Bloomery chassis: [`BloomeryChassis`] (issue #6244), the journal-driven
//! engine. Boots the shared base stratum plus the component host and the RPC
//! server, then the mount seam spawns the journal owner and the bundle driver
//! over one journal file.
//!
//! The composition is deliberately narrow: no egress, exec, TCP, or
//! HTTP-serving capability rides this engine (the zero-external-integration
//! rule), while the RPC server keeps it drivable over MCP (ADR-0155 §3).

use std::mem;
use std::sync::Arc;

use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, RegistryQueueConfig, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
    chassis_residual_knobs, install_frame_size, with_rpc_server,
};
use aether_chassis::cli::ChassisCli;
use aether_chassis::entry::ChassisEnv;
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_substrate::chassis::BootableChassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::composed;
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::{ConfigError, KnobRecord, validate_env};
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::cli::BloomeryCli;
use crate::config::BloomeryConfig;
use crate::driver::BloomeryDriverCapability;
use crate::mount::{self, Mounted};

/// Marker type for the bloomery chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<BloomeryChassis>`] returned
/// by `Self::build`. Same shape as the other chassis markers post
/// ADR-0071 phase 3.
pub struct BloomeryChassis;

impl Chassis for BloomeryChassis {
    const PROFILE: &'static str = "bloomery";
    type Driver = BloomeryDriverCapability;
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
    /// lower the bloomery knobs, stand up the substrate, re-apply the resolved
    /// log filter, lift the base out of the env, compose the shared stratum
    /// plus the component host and the RPC server, sweep for unknown env keys,
    /// install the signal-blocking driver, and mount the journal owner and the
    /// bundle driver before returning. The [`Mounted`] references come back
    /// beside the chassis for an embedder that drives it in process.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when the bloomery knobs do not lower, the
    /// substrate or the composed chain fails to boot, or either mount spawn
    /// fails.
    pub fn build_mounted(mut env: BloomeryEnv) -> Result<(BuiltChassis<Self>, Mounted), BootError> {
        // Lower the bloomery knobs first, before anything with a side effect:
        // an unset journal or an out-of-range closure limit is a typo in the
        // operator's own argv, and refusing it here costs nothing, where
        // refusing it at the mount seam would first stand up wasmtime and let
        // the RPC server bind and drop a port. `--describe` / `--print-config`
        // exit in `run_chassis_main`'s prelude before `build` is called, so
        // they never reach this and still answer with no journal configured.
        let (journal, limit) = mem::take(&mut env.bloomery).to_journal_and_limit()?;
        let mut boot = SubstrateBoot::build()?;
        apply_filter(&env.runtime.log_filter);
        let base = mem::take(&mut env.base);
        let builder = composed::<Self>(&mut boot, base, env)?;
        validate_env(&builder.config_manifest().known_keys(&chassis_residual_knobs()))?;
        let built = builder.driver(BloomeryDriverCapability { boot }).build()?;
        let mounted = mount::mount(&built, &journal, limit)?;
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
    /// The bloomery knobs. Lowered to the journal path and the driver's
    /// closure limit at the top of [`BloomeryChassis::build_mounted`], then applied off
    /// the builder at the mount seam.
    pub bloomery: BloomeryConfig,
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
        Ok(Self {
            base: ChassisBase { sources, actor_ring, scheduler_tuning, registry_queues, settlement },
            runtime,
            bloomery,
        })
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
    /// component host (which the driver's `Command::Load` targets), the RPC
    /// server (ADR-0155 §3), and the bloomery config declaration; the env
    /// carries values the delta resolves nothing from, so it takes no part.
    fn compose(builder: Builder<Self>, boot: &SubstrateBoot, _env: Self::Env) -> Result<Builder<Self>, BootError> {
        let component_host_params = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        Ok(with_rpc_server(builder.with_actor::<ComponentHostCapability>(component_host_params))
            .declare_config_member::<BloomeryConfig>())
    }
}

#[cfg(test)]
mod config_manifest_tests {
    use super::BloomeryChassis;
    use aether_chassis::boot::chassis_residual_knobs;
    use aether_substrate::chassis::config_manifest;

    #[test]
    fn bloomery_known_keys_claim_its_knobs_and_no_integration_caps() {
        // The aggregate is derived from what the bloomery actually composes: it
        // must claim its own two knobs plus the RPC port, and must not claim any
        // egress/exec/serving knob. Catches a later edit that composes
        // `with_full_stack_caps`, which would reintroduce egress/exec, and a
        // dropped `declare_config_member` that would make the journal knob warn
        // as unknown.
        let manifest = config_manifest::<BloomeryChassis>().expect("bloomery config manifest");
        let known = manifest.known_keys(&chassis_residual_knobs());
        assert!(known.contains("AETHER_BLOOMERY_JOURNAL"), "bloomery must claim its journal knob");
        assert!(known.contains("AETHER_BLOOMERY_CLOSURE_LIMIT_BYTES"), "bloomery must claim its closure-limit knob");
        assert!(known.contains("AETHER_RPC_PORT"), "bloomery must claim the RPC port via the composed RpcServerConfig");
        assert!(
            !known.contains("AETHER_HTTP_DISABLE"),
            "bloomery composes no http egress, so it must not claim the http knob"
        );
        assert!(
            !known.contains("AETHER_PROCESS_ALLOWLIST"),
            "bloomery composes no subprocess exec, so it must not claim the process knob"
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
