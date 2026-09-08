//! The harness chassis CLI root (ADR-0090 unit d, issue #5734). [`HarnessCli`]
//! flattens the overlays the harness chassis actually resolves — the fs roots,
//! the lifecycle cap knob, the four `ChassisBase` tuning members, and the two
//! render members the pumped `aether.render` actor takes — alongside the
//! source-selecting [`ChassisMeta`] flags. The shared staging / flag-naming /
//! help-forwarding machinery lives in `aether_chassis::cli`.
//!
//! Deliberately not `CommonOverlay`: the harness composes no worker pool
//! override, no HTTP / process caps and no boot-manifest autoload, and
//! advertising flags for knobs the chassis never resolves would fail boot as an
//! orphaned argv layer rather than quietly doing nothing (ADR-0156 §5).

use aether_chassis::boot::{ActorRingOverlay, RegistryQueueOverlay, SchedulerTuningOverlay, env_only_after_help};
use aether_chassis::chassis_cli;
use aether_chassis::cli::ChassisMeta;
use aether_fs::NamespaceRootsOverlay;
use aether_lifecycle::LifecycleOverlay;
use aether_render::RenderTuningOverlay;
use aether_substrate::config::SettlementOverlay;
use clap::Parser;

use crate::env::RenderSizeOverlay;

/// Harness chassis CLI root — the standalone form of the in-process
/// `SubstrateHarness`.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-substrate-harness",
    about = "Harness chassis — loopback-driven, offscreen render, deterministic advance. ADR-0067 / ADR-0161.",
    long_about = "Harness chassis — loopback-driven, offscreen render, deterministic advance. ADR-0067 / \
        ADR-0161.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = env_only_after_help()
)]
pub struct HarnessCli {
    /// `aether.fs` namespace roots: `--save-dir` / `--assets-dir` / `--config-dir`.
    #[command(flatten)]
    pub fs: NamespaceRootsOverlay,
    /// Lifecycle cap knob: `--lifecycle-advance-timeout-millis`.
    #[command(flatten)]
    pub lifecycle: LifecycleOverlay,

    /// Per-actor ring-capacity knobs (issue 1990): `--actor-*`.
    #[command(flatten)]
    pub actor_ring: ActorRingOverlay,
    /// Scheduler hot-path tuning knobs (issue 2485): `--scheduler-*`.
    #[command(flatten)]
    pub scheduler: SchedulerTuningOverlay,
    /// ADR-0165 serialized-queue bounds (issue 4122): `--registry-*-queue-capacity`.
    #[command(flatten)]
    pub registry_queues: RegistryQueueOverlay,
    /// Settlement-patience backstop (issue 2062): `--settlement-cap-secs`, which
    /// the harness resolves for both its settlement gates and its teardown budget.
    #[command(flatten)]
    pub settlement: SettlementOverlay,

    /// Render cap tuning for the pumped offscreen render actor:
    /// `--render-vertex-buffer-bytes` / `--render-clear-color` /
    /// `--render-pass-timings`.
    #[command(flatten)]
    pub render: RenderTuningOverlay,
    /// The harness's own offscreen size knob: `--substrate-harness-size WxH`.
    #[command(flatten)]
    pub render_size: RenderSizeOverlay,

    /// The source-selecting meta flags (`--config` / `--print-config` /
    /// `--describe`); see [`ChassisMeta`].
    #[command(flatten)]
    #[stage(skip)]
    pub meta: ChassisMeta,
}

chassis_cli!(HarnessCli {
    NamespaceRootsOverlay,
    LifecycleOverlay,
    ActorRingOverlay,
    SchedulerTuningOverlay,
    RegistryQueueOverlay,
    SettlementOverlay,
    RenderTuningOverlay,
    RenderSizeOverlay,
});
