//! The desktop chassis CLI root (ADR-0090 unit d, issue 1258). [`DesktopCli`]
//! composes the shared [`CommonOverlay`] full-stack cap bundle with the
//! desktop-only extras — audio, render tuning, window mode/title — and the
//! source-selecting [`ChassisMeta`] flags. The shared staging / flag-naming /
//! help-forwarding machinery lives in `aether_chassis::cli`.

use aether_audio::AudioOverlay;
use aether_chassis::boot::env_only_after_help;
use aether_chassis::cli::{ChassisCli, ChassisMeta, CommonOverlay};
use aether_chassis::window::WindowOverlay;
use aether_render::RenderTuningOverlay;
use clap::Parser;

/// Desktop chassis CLI root.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-substrate",
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

impl ChassisCli for DesktopCli {
    fn meta(&self) -> &ChassisMeta {
        &self.meta
    }
}

#[cfg(test)]
mod tests {
    //! Desktop root checkability (ADR-0156 §5): the hand-written root's long-flag
    //! set must equal the union of its composed overlays' flags plus the meta
    //! flags, so a dropped or stale flatten fails honestly.

    use super::DesktopCli;
    use aether_audio::AudioOverlay;
    use aether_chassis::cli::{CommonOverlay, long_flags, meta_flags, overlay_flags};
    use aether_chassis::window::WindowOverlay;
    use aether_render::RenderTuningOverlay;
    use clap::CommandFactory;

    #[test]
    fn desktop_root_flags_equal_composed_overlay_set() {
        let mut expected = overlay_flags::<CommonOverlay>();
        expected.extend(overlay_flags::<AudioOverlay>());
        // Desktop composes the wgpu render cap, so its `RenderTuningConfig` overlay
        // is flattened only here, not into the shared `CommonOverlay` (issue 3882).
        expected.extend(overlay_flags::<RenderTuningOverlay>());
        expected.extend(overlay_flags::<WindowOverlay>());
        expected.extend(meta_flags());
        assert_eq!(long_flags(&DesktopCli::command()), expected);
    }
}
