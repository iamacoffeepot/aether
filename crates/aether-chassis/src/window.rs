//! Desktop window boot knobs. Declared here (not in the desktop
//! chassis) because the fleet-wide config registry in [`crate::boot`]
//! and the shared CLI roots in [`crate::cli`] both name the derived
//! `WindowConfigLayer` / `WindowOverlay`. The winit lowering
//! (`resolve_fullscreen`, video-mode matching) stays desktop-side —
//! this module is env/argv grammar only.

use std::io;

use aether_kinds::WindowMode;
use aether_substrate::config::ConfigError;

/// Desktop window boot knobs (ADR-0090 §1/§2 applied to the chassis's
/// own window knobs). The `#[derive(aether_substrate::Config)]` emits
/// the env-shaped `WindowConfigLayer`, the clap-shaped `WindowOverlay`,
/// the `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims — mirrors `ActorRingConfig` in [`crate::boot`].
///
/// `env_prefix = "AETHER_WINDOW"` joins the field env keys — `mode` →
/// `AETHER_WINDOW_MODE`, `title` → `AETHER_WINDOW_TITLE` — matching the
/// historical key names exactly without per-field overrides. Both fields
/// are `Option<String>` (soft: missing or empty → `None`).
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_WINDOW", cli_prefix = "window")]
pub struct WindowConfig {
    /// Window mode at boot: windowed, fullscreen-borderless, or exclusive fullscreen.
    ///
    /// Accepts `windowed`, `windowed:WxH`, `fullscreen-borderless`, or
    /// `exclusive:WxH@HZ`. Lowered via [`Self::lower`], which delegates to
    /// [`parse_window_mode_env`]; a present-but-unparseable value hard-errors
    /// the boot (ADR-0090 §4), while an absent value resolves to `Windowed`.
    pub mode: Option<String>,
    /// Window title at boot; unset uses "aether".
    ///
    /// Lowered via [`Self::lower`]; empty / unset → `"aether"`.
    pub title: Option<String>,
    /// GPU wireframe mode at boot: off (default), line, or overlay.
    ///
    /// The env key is pinned back to `AETHER_WIREFRAME` (not
    /// `AETHER_WINDOW_WIREFRAME`) and the argv flag to `--wireframe` (not
    /// `--window-wireframe`) since the knob predates joining
    /// `WindowConfig`. Threaded verbatim to the desktop render driver's
    /// `WireframeMode::from_config_value`, which owns the tri-state parse.
    #[config(env = "AETHER_WIREFRAME", cli_long = "wireframe")]
    pub wireframe: Option<String>,
}

/// The typed settings the window opens with — resolved in the desktop
/// `Chassis::build` off the source stack and threaded to the desktop driver.
/// Mirrors the other embedded knob groups (`RingCapacities`,
/// `SchedulerTuning`, `RenderTuningConfig`) rather than riding as loose
/// fields. Produced by [`WindowConfig::lower`].
#[derive(Clone, Debug)]
pub struct WindowSettings {
    /// Desktop window mode at boot, lowered from `WindowConfig::mode`.
    pub mode: WindowMode,
    /// Initial windowed size (`Some` only for a `windowed:WxH` mode).
    pub size: Option<(u32, u32)>,
    /// Window title at boot; `"aether"` when unset or empty.
    pub title: String,
    /// Resolved `AETHER_WIREFRAME` config value, threaded verbatim to the
    /// desktop render driver's `WireframeMode::from_config_value`.
    pub wireframe: Option<String>,
}

impl WindowConfig {
    /// Lower the resolved window knobs into the [`WindowSettings`] unit the
    /// desktop `Chassis::build` threads to the driver. Subsumes the mode + title lowering:
    /// `mode` delegates to [`parse_window_mode_env`], and a present-but-bad
    /// `AETHER_WINDOW_MODE` value hard-errors the boot (ADR-0090 §4) rather
    /// than silently defaulting; an absent value resolves to `Windowed`.
    /// `title` maps `None` (unset or empty — the derive filters empty →
    /// `None`) to `"aether"` and passes a provided value through verbatim;
    /// `wireframe` rides through unchanged.
    ///
    /// # Errors
    ///
    /// Returns a hard [`ConfigError`] naming the offending value and the
    /// accepted grammar when `mode` is present but unparseable.
    pub fn lower(self) -> Result<WindowSettings, ConfigError> {
        let (mode, size) = match self.mode.as_deref() {
            None => (WindowMode::Windowed, None),
            Some(s) => parse_window_mode_env(s).map_err(|e| {
                ConfigError::unparseable(
                    "AETHER_WINDOW_MODE",
                    s,
                    io::Error::other(format!(
                        "{e}; accepted: windowed[:WxH] | fullscreen-borderless | exclusive:WxH@HZ"
                    )),
                )
            })?,
        };
        let title = self.title.unwrap_or_else(|| "aether".to_owned());

        Ok(WindowSettings { mode, size, title, wireframe: self.wireframe })
    }
}

/// Parse `AETHER_WINDOW_MODE`. Grammar:
///   `windowed`              — default size
///   `windowed:WxH`          — windowed, `WxH` physical pixels
///   `fullscreen-borderless` — borderless on current monitor
///   `exclusive:WxH@HZ`      — exclusive, matched against monitor modes
/// Refresh is integer Hz (converted to mhz by *1000); non-integer
/// refresh isn't expressible from the env var today — runtime
/// `set_window_mode` accepts full-precision mhz directly.
pub fn parse_window_mode_env(s: &str) -> Result<(WindowMode, Option<(u32, u32)>), String> {
    let s = s.trim();
    if s == "windowed" {
        return Ok((WindowMode::Windowed, None));
    }
    if let Some(rest) = s.strip_prefix("windowed:") {
        let (w, h) = parse_wxh(rest)?;
        return Ok((WindowMode::Windowed, Some((w, h))));
    }
    if s == "fullscreen-borderless" {
        return Ok((WindowMode::FullscreenBorderless, None));
    }
    if let Some(rest) = s.strip_prefix("exclusive:") {
        let (dim, hz) = rest.split_once('@').ok_or_else(|| format!("exclusive mode missing @HZ in {s:?}"))?;
        let (width, height) = parse_wxh(dim)?;
        let hz: u32 = hz.parse().map_err(|e| format!("invalid Hz {hz:?}: {e}"))?;
        return Ok((WindowMode::FullscreenExclusive { width, height, refresh_mhz: hz.saturating_mul(1000) }, None));
    }
    Err(format!("unrecognised AETHER_WINDOW_MODE value {s:?}"))
}

fn parse_wxh(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or_else(|| format!("expected WxH, got {s:?}"))?;
    let w: u32 = w.parse().map_err(|e| format!("invalid width {w:?}: {e}"))?;
    let h: u32 = h.parse().map_err(|e| format!("invalid height {h:?}: {e}"))?;
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_title_none_returns_default() {
        // Unset title → "aether" default.
        assert_eq!(WindowConfig::default().lower().expect("default config lowers cleanly").title, "aether");
    }

    #[test]
    fn lower_title_some_returns_value() {
        // Provided title passes through verbatim.
        let cfg = WindowConfig { mode: None, title: Some("my game".to_owned()), wireframe: None };
        assert_eq!(cfg.lower().expect("no mode set, so lowering succeeds").title, "my game");
    }

    #[test]
    fn lower_bad_mode_hard_errors_naming_the_value() {
        // A present-but-unparseable AETHER_WINDOW_MODE aborts the boot (ADR-0090
        // §4) instead of silently falling back to Windowed. The rendered error
        // must name the offending value so the operator can spot the typo.
        let cfg = WindowConfig { mode: Some("windoze".to_owned()), title: None, wireframe: None };
        let err = cfg.lower().expect_err("a bad window mode must be a hard config error");
        let rendered = err.to_string();
        assert!(rendered.contains("windoze"), "error must name the offending value, got: {rendered}");
        assert!(rendered.contains("fullscreen-borderless"), "error must name the accepted grammar, got: {rendered}");
    }

    #[test]
    fn parse_windowed_defaults() {
        let (m, s) = parse_window_mode_env("windowed").expect("test setup: \"windowed\" is a valid spec");
        assert!(matches!(m, WindowMode::Windowed));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_windowed_with_size() {
        let (m, s) = parse_window_mode_env("windowed:1280x720").expect("test setup: \"windowed:WxH\" is a valid spec");
        assert!(matches!(m, WindowMode::Windowed));
        assert_eq!(s, Some((1280, 720)));
    }

    #[test]
    fn parse_fullscreen_borderless() {
        let (m, s) = parse_window_mode_env("fullscreen-borderless")
            .expect("test setup: \"fullscreen-borderless\" is a valid spec");
        assert!(matches!(m, WindowMode::FullscreenBorderless));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_exclusive_converts_hz_to_mhz() {
        let (m, s) =
            parse_window_mode_env("exclusive:1920x1080@60").expect("test setup: \"exclusive:WxH@HZ\" is a valid spec");
        let WindowMode::FullscreenExclusive { width, height, refresh_mhz } = m else {
            panic!("expected exclusive");
        };
        assert_eq!((width, height, refresh_mhz), (1920, 1080, 60_000));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_rejects_unknown_variant() {
        assert!(parse_window_mode_env("garbage").is_err());
        assert!(parse_window_mode_env("exclusive:1920x1080").is_err());
        assert!(parse_window_mode_env("windowed:notxwide").is_err());
    }

    #[test]
    fn parse_ignores_whitespace() {
        let (m, _) = parse_window_mode_env("  windowed  ").expect("test setup: surrounding whitespace is trimmed");
        assert!(matches!(m, WindowMode::Windowed));
    }
}
