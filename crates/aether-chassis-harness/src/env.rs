//! The harness chassis env (ADR-0162, issue #5734): the resolved config data
//! the standalone harness binary boots from, plus the one knob it owns
//! (`AETHER_SUBSTRATE_HARNESS_SIZE`).
//!
//! Every field resolves through the argv/env/file source stack the CLI root
//! assembles, so `--describe` / `--print-config` resolve exactly what a boot
//! resolves and the unknown-`AETHER_*` sweep sees the whole surface. Before
//! this the binary read `RenderSizeConfig::from_env()` /
//! `RenderTuningConfig::from_env()` / `NamespaceRoots::from_env()` directly,
//! which is the same values by a path no manifest could see.

use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, RegistryQueueConfig, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
    install_frame_size,
};
use aether_chassis::cli::ChassisCli;
use aether_fs::NamespaceRoots;
use aether_render::RenderTuningConfig;
use aether_substrate::config::ConfigError;
use aether_substrate_harness_cap::events::{self, EventReceiver, EventSender};

/// The offscreen render size the binary falls back to when
/// `AETHER_SUBSTRATE_HARNESS_SIZE` is unset or unparseable — re-exported from
/// the in-process harness's builder default so both forms of the harness render
/// at the same size rather than pinning the pair twice.
pub use aether_harness_substrate::{DEFAULT_HEIGHT, DEFAULT_WIDTH};

/// Render-size knob for the harness binary (`AETHER_SUBSTRATE_HARNESS_SIZE=WxH`):
/// a `#[derive(aether_substrate::Config)]` struct resolved off the chassis
/// source stack and lowered to `(u32, u32)` by [`Self::to_size`]. Binary-side
/// because the in-process harness sizes through its builder, not process env —
/// issue #5706 moved it out of `aether-chassis`, whose only reason to hold it
/// was the harness dependency this crate owns anyway.
///
/// The explicit `env =` pin is belt-and-suspenders against a future field
/// rename, matching how `ActorRingConfig` pins its historical keys.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_SUBSTRATE_HARNESS", cli_prefix = "substrate-harness")]
pub struct RenderSizeConfig {
    /// Offscreen render width and height in pixels; unset falls back to 800x600.
    ///
    /// Render dimensions for the offscreen wgpu surface, given as
    /// `width x height`. Falls back to `800x600` on missing/unparseable
    /// input with a warn log.
    #[config(env = "AETHER_SUBSTRATE_HARNESS_SIZE")]
    pub size: Option<String>,
}

impl RenderSizeConfig {
    /// Lower the resolved knob to `(width, height)` pixels: missing value,
    /// missing `x` separator, non-numeric parts, or a zero dimension all fall
    /// back to [`DEFAULT_WIDTH`] × [`DEFAULT_HEIGHT`] with a `warn` log.
    #[must_use]
    pub fn to_size(&self) -> (u32, u32) {
        let Some(raw) = self.size.as_deref() else {
            return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
        };
        if let Some((width, height)) = raw.split_once('x') {
            match (width.parse::<u32>(), height.parse::<u32>()) {
                (Ok(width), Ok(height)) if width > 0 && height > 0 => (width, height),
                _ => {
                    tracing::warn!(
                        target: "aether_chassis_harness::boot",
                        value = %raw,
                        "AETHER_SUBSTRATE_HARNESS_SIZE unparseable — falling back to default",
                    );
                    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
                }
            }
        } else {
            tracing::warn!(
                target: "aether_chassis_harness::boot",
                value = %raw,
                "AETHER_SUBSTRATE_HARNESS_SIZE missing 'x' separator — falling back to default",
            );
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        }
    }
}

/// The harness chassis env: the resolved config the standalone binary composes
/// and drives from. Mirrors the full-stack chassis's `CommonEnv` in shape (a
/// [`ChassisBase`] stratum plus the members the chassis itself consumes) while
/// carrying only the surface the harness actually has — no worker-pool knob, no
/// boot-manifest autoload, since it composes neither.
pub struct HarnessEnv {
    /// The universal base stratum (config source stack + the non-cap ring /
    /// scheduler / registry-queue / settlement members) `composed` installs
    /// ahead of `compose`. Lifted out with `mem::take` at the boot seam, exactly
    /// as the full-stack chassis do.
    pub base: ChassisBase,
    /// The resolved `aether.fs` namespace roots. Resolved chassis-side and
    /// passed to the fs cap programmatically so the cap uses the exact same
    /// value — and so the `assets` root can thread into the pumped render
    /// actor's `capture_frame` similarity references.
    pub namespace_roots: NamespaceRoots,
    /// The substrate runtime knobs (#3849). Only `log_filter` is consumed here
    /// (re-applied once the subscriber is installed); the field carries the
    /// whole resolved member so its `[runtime]` file / env values resolve once.
    pub runtime: RuntimeConfig,
    /// Render tuning for the pumped `aether.render` actor the binary boots
    /// post-`build_passive` (ADR-0161). The pumped path composes no build-time
    /// render cap, so the chassis resolves this member itself and declares it on
    /// the aggregate rather than letting `with_actor` resolve it.
    pub render: RenderTuningConfig,
    /// The offscreen size the pumped render actor boots at.
    pub render_size: RenderSizeConfig,
    /// Sender half of the chassis event channel: cloned into the
    /// `aether.substrate_harness` cap's params at compose, and into the pumped
    /// render slot's wake by `main`. The matching receiver rides out of
    /// [`Self::resolve`].
    pub events: EventSender,
}

impl HarnessEnv {
    /// Resolve the harness env off the source stack the CLI root assembles,
    /// paired with the receiver half of the chassis event channel the binary's
    /// loop drives. The single env-reading edge (ADR-0070): `--describe` and
    /// `--print-config` resolve it exactly as a boot does.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the `--config` file cannot be read, or when
    /// a known `AETHER_*` member (or argv overlay value) holds an unparseable
    /// value (ADR-0090 §4).
    pub fn resolve(cli: impl ChassisCli) -> Result<(Self, EventReceiver), ConfigError> {
        let mut sources = cli.into_sources()?;

        let namespace_roots = sources.resolve::<NamespaceRoots>()?;
        let actor_ring = sources.resolve::<ActorRingConfig>()?;
        let scheduler_tuning = sources.resolve::<SchedulerTuningConfig>()?;
        let registry_queues = sources.resolve::<RegistryQueueConfig>()?;
        let settlement = sources.resolve::<SettlementConfig>()?;
        let runtime = sources.resolve::<RuntimeConfig>()?;
        // The pumped render actor is booted post-build rather than composed, so
        // its two members resolve here; `HarnessChassis::compose` declares both
        // on the aggregate so their keys stay known and their argv layers are
        // consumed rather than orphaned (ADR-0156 §5).
        let render = sources.resolve::<RenderTuningConfig>()?;
        let render_size = sources.resolve::<RenderSizeConfig>()?;
        // ADR-0156 §6 (#3850): push the resolved wire-frame cap into the codec
        // before any framing runs — the codec cannot pull the knob itself.
        install_frame_size(&mut sources)?;

        let base = ChassisBase { sources, actor_ring, scheduler_tuning, registry_queues, settlement };
        let (events, events_rx) = events::channel();
        Ok((Self { base, namespace_roots, runtime, render, render_size, events }, events_rx))
    }
}
