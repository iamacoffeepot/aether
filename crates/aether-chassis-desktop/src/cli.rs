//! The desktop chassis CLI root (ADR-0090 unit d, issue 1258). [`DesktopCli`]
//! composes the shared [`CommonOverlay`] full-stack cap bundle with the
//! desktop-only extras — audio, render tuning, window mode/title — and the
//! source-selecting [`ChassisMeta`] flags. The shared staging / flag-naming /
//! help-forwarding machinery lives in `aether_chassis::cli`.

use aether_audio::AudioOverlay;
use aether_chassis::boot::env_only_after_help;
use aether_chassis::chassis_cli;
use aether_chassis::cli::{ChassisMeta, CommonOverlay};
use aether_chassis::window::WindowOverlay;
use aether_render::RenderTuningOverlay;
use clap::Parser;

/// Desktop chassis CLI root.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-desktop",
    about = "Desktop chassis — winit window + wgpu render + cpal audio. ADR-0035 / ADR-0090.",
    long_about = "Desktop chassis — winit window + wgpu render + cpal audio. ADR-0035 / ADR-0090.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = env_only_after_help()
)]
pub struct DesktopCli {
    #[command(flatten)]
    pub common: CommonOverlay,
    #[command(flatten)]
    pub audio: AudioOverlay,
    /// Render cap tuning (desktop composes the wgpu render cap):
    /// `--render-vertex-buffer-bytes`, shadowing `AETHER_RENDER_VERTEX_BUFFER_BYTES`
    /// (issue 3882 flattened its overlay here; headless composes the nop render cap,
    /// which resolves no `RenderTuningConfig`, so it carries no render flag).
    #[command(flatten)]
    pub render: RenderTuningOverlay,
    /// Desktop window knobs: `--window-mode`, `--window-title`.
    #[command(flatten)]
    pub window: WindowOverlay,

    /// The source-selecting meta flags (`--config` / `--print-config` /
    /// `--describe`); see [`ChassisMeta`].
    #[command(flatten)]
    #[stage(skip)]
    pub meta: ChassisMeta,
}

// Desktop composes the wgpu render cap, so its `RenderTuningConfig` overlay is
// flattened only here, not into the shared `CommonOverlay` (issue 3882).
chassis_cli!(DesktopCli { CommonOverlay, AudioOverlay, RenderTuningOverlay, WindowOverlay });
