use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aether_data::KindId;
use aether_substrate::config::{ConfigError, ConfigProvenance, ConfigSources};

/// The historical dark field, kept as the default so nothing but a line
/// drawing has to opt out of it.
///
/// Spelled as the sRGB value a capture reads back (`63, 75, 97`), which is
/// what the field has always displayed: the colour targets are sRGB, so a
/// clear is stored encoded, and the older `0d1220` literal was handed to
/// the GPU as if it were linear. The knob now decodes like every other
/// colour an author writes, and this literal keeps the shipped default
/// pixel-identical across that change.
pub const DEFAULT_CLEAR_COLOR: &str = "3f4b61";

/// Boot knobs for `RenderCapability` (ADR-0090). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `RenderTuningConfigLayer`, the clap-shaped `RenderTuningOverlay`,
/// the `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims — mirrors `AudioConfig`. This is the cap's
/// operator-resolvable `Config` (ADR-0156 §3); the non-knob wiring
/// (chassis-derived assets root, test observability) rides the separate
/// [`RenderParams`] channel instead.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_RENDER", cli_prefix = "render")]
pub struct RenderTuningConfig {
    /// Per-frame vertex buffer size in bytes; frames beyond it are truncated.
    ///
    /// The size the GPU vertex buffer is created with and the byte count
    /// the render accumulator truncates to (with a warn) when a frame's
    /// triangles exceed it. Default
    /// [`VERTEX_BUFFER_BYTES`](aether_substrate::render::VERTEX_BUFFER_BYTES)
    /// (64 MiB, ~932k triangles at 72 bytes each).
    #[config(default = 67_108_864)]
    pub vertex_buffer_bytes: usize,
    /// Background the colour pass clears to, as sRGB `rrggbb` hex.
    ///
    /// A knob rather than a constant because what the background should be
    /// is a property of what is being drawn, not of the renderer: a lit 3D
    /// scene wants the dark default it has always had, and a line drawing
    /// wants paper — on a dark field, ink is invisible and pale hatching
    /// reads as highlight, which inverts the whole tonal reading. A depot
    /// package carries it as a chassis setting
    /// ([`apply_manifest_clear_color`]), so a shipped line drawing comes up
    /// on paper with no flags. The hex is the colour a capture reads back:
    /// `f6f2e9` clears to `f6f2e9`.
    #[config(default = "3f4b61")]
    pub clear_color: String,
    /// Measure per-pass GPU durations for authored programs.
    ///
    /// Brackets every recorded program pass with a wgpu timestamp query
    /// pair and folds the resolved spans into the per-pass EWMAs
    /// `aether.render.program.timings` reports
    /// (iamacoffeepot/aether#4423). Off by default: placing a timestamp
    /// at a pass boundary is not free on a tile-based GPU, and a
    /// measurement that changes the frame it measures should be asked
    /// for rather than assumed. Turning it on where the adapter has no
    /// `TIMESTAMP_QUERY` changes nothing — the reply stays
    /// absent-with-reason.
    #[config(default = false)]
    pub pass_timings: bool,
}

/// sRGB `rrggbb` to the linear RGB a clear takes, falling back to the
/// historical dark field when the string is not six hex digits.
///
/// Linear because that is what `wgpu::Color` means against an sRGB target:
/// the store encodes it, so a hex decoded here reads back as itself. Feeding
/// the channels through undecoded (the older shape) displayed every value
/// brighter than written — `0d1220` came up as `3f4b61`.
#[must_use]
pub fn parse_clear_color(hex: &str) -> [f64; 3] {
    let channel = |at: usize| u8::from_str_radix(hex.get(at..at + 2).unwrap_or("!!"), 16).ok();

    match (channel(0), channel(2), channel(4)) {
        (Some(r), Some(g), Some(b)) if hex.len() == 6 => [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b)],
        _ => [0.05, 0.07, 0.12],
    }
}

/// One 8-bit sRGB channel to linear, by the IEC 61966-2-1 transfer curve
/// (the same decode the GPU applies when sampling an `Srgb` texture).
fn srgb_to_linear(channel: u8) -> f64 {
    let encoded = f64::from(channel) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Overlay a depot package manifest's `clear_color` onto the config stack
/// BELOW argv/env/file, ABOVE the compiled default — the precedence a
/// package's window mode and tick cadence take (issue 4001).
///
/// A shipped line drawing has to come up on paper with no flags, and the
/// operator has to keep `AETHER_RENDER_CLEAR_COLOR` / `--render-clear-color`
/// over it for debugging, so the manifest slots in one layer lower. The
/// mechanism mirrors the tick seam: ask the stack which layer supplies
/// [`RenderTuningConfig`], and only when that is
/// [`ConfigProvenance::Default`] substitute the manifest's colour into the
/// resolved value, re-staged as the programmatic override the desktop
/// `Chassis::build` resolves. Provenance is member-granular, so an operator
/// who pins any render knob keeps the compiled clear colour too — the side
/// that errs toward the operator.
///
/// `None` (a manifest carrying no colour, or a boot with no package) leaves
/// the stack untouched.
///
/// # Errors
///
/// Propagates the [`ConfigError`] from resolving [`RenderTuningConfig`] off
/// the stack when a known `AETHER_RENDER_*` value is malformed (ADR-0090 §4).
pub fn apply_manifest_clear_color(sources: &mut ConfigSources, clear_color: Option<&str>) -> Result<(), ConfigError> {
    let Some(manifest) = clear_color else {
        return Ok(());
    };

    // Read the provenance before resolving: resolution consumes the staged
    // argv layer and programmatic override, so asking afterwards would report
    // `Default` for a value those layers supplied.
    let supplied = sources.provenance_of::<RenderTuningConfig>() != ConfigProvenance::Default;
    let mut resolved = sources.resolve::<RenderTuningConfig>()?;
    if !supplied {
        manifest.clone_into(&mut resolved.clear_color);
    }
    sources.set_override(resolved);
    Ok(())
}

/// Composer-supplied construction params for `RenderCapability`
/// (ADR-0156 §3): the non-knob wiring the chassis computes at boot, kept
/// off the operator-resolvable [`RenderTuningConfig`] `Config`.
///
/// `observed_kinds`, when set, has every successfully-dispatched
/// inbound mail's kind id pushed to it from the cap's `#[handler]`
/// methods — used by the in-process substrate-harness to assert what kinds
/// the cap has seen. Production chassis leave it `None` (zero
/// overhead). Decode failures and unknown kinds don't push (the
/// macro miss path warn-logs at the chassis-side dispatcher and
/// short-circuits before any handler runs).
#[derive(Clone, Default)]
pub struct RenderParams {
    /// `SubstrateHarness` observation sink.
    pub observed_kinds: Option<Arc<Mutex<Vec<KindId>>>>,
    /// Resolved path for the `"assets"` namespace, used by the
    /// `capture_frame` handler to read reference images for similarity
    /// checks (iamacoffeepot/aether#1780). The handler resolves the
    /// reference PNG synchronously and passes the raw bytes through the
    /// pending capture. `None` disables similarity checks with a
    /// descriptive `Err` reply.
    pub assets_dir: Option<PathBuf>,
    /// ADR-0161 slice R4: offscreen boot dimensions for a surfaceless
    /// runtime (the substrate harness). `Some((w, h))` makes the lazy
    /// `on_frame` boot stand up a surfaceless GPU at these dimensions when
    /// no window target is requested; `None` leaves the runtime windowed
    /// (or never booted, in a no-GPU test).
    pub offscreen_size: Option<(u32, u32)>,
    /// Resolved `AETHER_WIREFRAME` value (argv > env > default), threaded
    /// so the lazy wgpu boot picks the wireframe mode. `None` / `"off"` is
    /// filled faces.
    pub wireframe: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_field_decodes_to_the_field_it_always_was() {
        // Tripwire: the compiled default is the sRGB spelling of the linear
        // `0.05, 0.07, 0.12` every capture reference was scored against, so
        // drift here changes the readback pixel of every scene that never
        // asked for a background. Drifts when the default literal or the
        // transfer curve changes.
        let [r, g, b] = parse_clear_color(DEFAULT_CLEAR_COLOR);
        assert!((r - 0.05).abs() < 0.001 && (g - 0.07).abs() < 0.001 && (b - 0.12).abs() < 0.001, "{r} {g} {b}");
    }

    #[test]
    fn hex_decodes_through_the_srgb_curve_and_malformed_falls_back() {
        // The decode is the logic this crate owns: both ends of the curve
        // (the linear toe and the power segment) and the fall-back for a
        // string that is not six hex digits.
        assert_eq!(parse_clear_color("ffffff"), [1.0, 1.0, 1.0]);
        assert_eq!(parse_clear_color("000000"), [0.0, 0.0, 0.0]);
        let [toe, _, _] = parse_clear_color("050000");
        assert!((toe - 5.0 / 255.0 / 12.92).abs() < 1e-9, "the toe is linear: {toe}");
        assert_eq!(parse_clear_color("f6f2e"), [0.05, 0.07, 0.12], "five digits fall back");
        assert_eq!(parse_clear_color("zzzzzz"), [0.05, 0.07, 0.12], "non-hex falls back");
    }

    #[test]
    fn a_manifest_colour_fills_the_default_and_yields_to_a_pin() {
        // Manifest beats default, pin beats manifest (issue 4001 precedence):
        // a depot's paper reaches the resolved config when nothing above the
        // compiled default supplied a render knob, and an operator's
        // programmatic override keeps its own colour. Hermetic sources so
        // the resolve reads no env.
        let mut sources = ConfigSources::hermetic();
        apply_manifest_clear_color(&mut sources, Some("f6f2e9")).expect("apply");
        let resolved = sources.resolve::<RenderTuningConfig>().expect("resolve");
        assert_eq!(resolved.clear_color, "f6f2e9", "the manifest colour fills the default");

        let mut sources = ConfigSources::hermetic();
        sources.set_override(RenderTuningConfig {
            vertex_buffer_bytes: 1024,
            clear_color: "101010".to_owned(),
            pass_timings: false,
        });
        apply_manifest_clear_color(&mut sources, Some("f6f2e9")).expect("apply over a pin");
        let resolved = sources.resolve::<RenderTuningConfig>().expect("resolve");
        assert_eq!(resolved.clear_color, "101010", "an operator pin beats the manifest");

        let mut sources = ConfigSources::hermetic();
        apply_manifest_clear_color(&mut sources, None).expect("apply nothing");
        let resolved = sources.resolve::<RenderTuningConfig>().expect("resolve");
        assert_eq!(resolved.clear_color, DEFAULT_CLEAR_COLOR, "no manifest colour leaves the default");
    }
}
