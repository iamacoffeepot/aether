//! The harness chassis (ADR-0067 / ADR-0161, issue #5734): the standalone
//! binary form of the in-process `SubstrateHarness`, on the shared
//! `aether-chassis` composition layer.
//!
//! [`HarnessChassis`] is a passive chassis — `main()` is the driver, the pump
//! host for the offscreen `aether.render` slot (see [`crate::pump`]) — but it is
//! a [`BootableChassis`] like every other chassis binary, so it gets the ADR-0162
//! ceremony the binary previously had none of: `--describe` (ADR-0115) and
//! `--print-config` (ADR-0090 §4) off the shared prelude, the composed
//! known-key sweep, the `ChassisBase` stratum, and one composition declaration
//! that both the boot path and the discovery paths run.
//!
//! The in-process sibling `SubstrateHarnessChassis::build_passive` stays exactly
//! where it is: it composes per-scenario over a hermetic source stack with no
//! aborter, which is the opposite of what a binary wants, so the two share the
//! harness *cap set* by convergent declaration rather than by routing the
//! deliberate embedder through a chassis ceremony it does not want.

use std::io;
use std::mem;
use std::sync::Arc;

use aether_chassis::boot::{ChassisBase, chassis_residual_knobs};
use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_fs::FsCapability;
use aether_lifecycle::{LifecycleCapability, frame_lifecycle_params};
use aether_render::RenderTuningConfig;
use aether_substrate::chassis::BootableChassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::{ConfigError, KnobRecord};
use aether_substrate::{Chassis, SubstrateBoot};
use aether_substrate_harness_cap::{SubstrateHarnessCapParams, SubstrateHarnessCapability};
use aether_tcp::TcpCapability;
use aether_text::TextCapability;
use aether_window::SyntheticWindowCapability;

use crate::cli::HarnessCli;
use crate::env::{HarnessEnv, RenderSizeConfig};

/// Marker type for the harness chassis. Carries no fields — the chassis
/// instance is the `PassiveChassis<HarnessChassis>` the binary builds and then
/// drives itself.
pub struct HarnessChassis;

impl Chassis for HarnessChassis {
    const PROFILE: &'static str = "substrate-harness";
    /// Phantom driver — the harness is passive (the binary's own loop is the
    /// driver). Declaring [`NeverDriver`] satisfies the trait bound; the value
    /// is never instantiated because the boot path ends in
    /// `Builder::build_passive`.
    type Driver = NeverDriver;
    type Env = HarnessEnv;

    /// Inert by design — the harness is a passive chassis, so there is no
    /// driver for the framework to run. The binary composes through
    /// [`composed`](aether_substrate::chassis::composed) and terminates in
    /// `build_passive`, then owns the pump loop itself. The trait method exists
    /// so `Builder<HarnessChassis>` can parameterise over `Chassis` per
    /// ADR-0071.
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Err(BootError::Other(Box::new(io::Error::other(
            "HarnessChassis has no driver; the aether-substrate-harness binary composes it and drives the pump loop \
             itself (see crate::pump)",
        ))))
    }
}

impl BootableChassis for HarnessChassis {
    type Base = ChassisBase;

    /// Resolve the harness env off the source stack (ADR-0162): the lone
    /// per-chassis token is the `HarnessCli` type.
    ///
    /// `--describe` / `--print-config` compose the same chain a boot composes
    /// but never dispatch, so the receiver half of the chassis event channel has
    /// no consumer on this path; dropping it leaves the harness cap's sender
    /// inert, which is exactly right for a claim-only pass.
    fn resolve_env() -> Result<(Self::Base, Self::Env), ConfigError> {
        let (mut env, _events_rx) = HarnessEnv::resolve(HarnessCli::default())?;
        let base = mem::take(&mut env.base);
        Ok((base, env))
    }

    fn residual_knobs() -> Vec<KnobRecord> {
        chassis_residual_knobs()
    }

    /// Compose the harness capability chain — the single claim/build path
    /// (ADR-0155) the binary's boot and the discovery helpers both run.
    ///
    /// Boot order is declaration order, and it is the order the in-process
    /// harness composes its own basics in: the component host first, then the
    /// caps a driven harness answers mail on, the synthetic window, the
    /// `aether.substrate_harness` advance cap, the frame lifecycle graph, and
    /// the fs roots last. The trace dispatcher and the four non-cap tuning
    /// members arrive ahead of all of it from [`ChassisBase`].
    ///
    /// The pumped `aether.render` actor is deliberately absent: ADR-0161 has the
    /// embedder claim that slot post-build so the offscreen GPU lives on the
    /// pump thread. Its two config members are therefore *declared* rather than
    /// composed — the chassis resolved them in [`HarnessEnv::resolve`], so
    /// declaring them here is what keeps their keys in the known-key sweep and
    /// their flags out of the orphaned-argv error.
    fn compose(builder: Builder<Self>, boot: &SubstrateBoot, env: Self::Env) -> Result<Builder<Self>, BootError> {
        let HarnessEnv { base: _, namespace_roots, runtime: _, render: _, render_size: _, events } = env;

        Ok(builder
            .with_actor::<ComponentHostCapability>(ComponentHostParams {
                engine: Arc::clone(&boot.engine),
                linker: Arc::clone(&boot.linker),
                hub_outbound: Arc::clone(&boot.outbound),
            })
            .with_actor::<TcpCapability>(())
            .with_actor::<TextCapability>(())
            .with_actor::<ClipboardCapability>(ClipboardParams::InMemory)
            .with_actor::<SyntheticWindowCapability>(())
            .with_actor::<SubstrateHarnessCapability>(SubstrateHarnessCapParams { events })
            .with_actor::<LifecycleCapability>(frame_lifecycle_params())
            // Programmatic: the fs cap uses the exact roots resolved chassis-side.
            .with_actor_configured::<FsCapability>((), namespace_roots)
            .declare_config_member::<RenderTuningConfig>()
            .declare_config_member::<RenderSizeConfig>())
    }
}
