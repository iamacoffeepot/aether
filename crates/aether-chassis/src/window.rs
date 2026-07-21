//! Desktop window boot knobs. Declared here (not in the desktop
//! chassis) because the fleet-wide config registry in [`crate::boot`]
//! and the shared CLI roots in [`crate::cli`] both name the derived
//! `WindowConfigLayer` / `WindowOverlay`. The winit lowering
//! (`resolve_fullscreen`, video-mode matching) stays desktop-side —
//! this module is env/argv grammar only.

use aether_kinds::WindowMode;

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
    /// `AETHER_WINDOW_MODE=<value>` desktop window mode at boot:
    /// `windowed[:WxH]` / `fullscreen-borderless` / `exclusive:WxH@HZ`.
    /// Lowered via [`Self::lower`] which delegates to
    /// [`parse_window_mode_env`] and soft-falls back to `Windowed` on
    /// a bad value (keeping the pre-migration behaviour).
    pub mode: Option<String>,
    /// `AETHER_WINDOW_TITLE=<text>` desktop window title at boot.
    /// Lowered via [`Self::lower`]; empty / unset → `"aether"`.
    pub title: Option<String>,
    /// `AETHER_WIREFRAME=<value>` desktop GPU wireframe mode at boot:
    /// `off` (default) / `line` / `overlay`. The env key is pinned back
    /// to `AETHER_WIREFRAME` (not `AETHER_WINDOW_WIREFRAME`) and the
    /// argv flag to `--wireframe` (not `--window-wireframe`) since the
    /// knob predates joining `WindowConfig`. Threaded verbatim to
    /// the desktop render driver's `WireframeMode::from_config_value`,
    /// which owns the tri-state parse.
    #[config(env = "AETHER_WIREFRAME", cli_long = "wireframe")]
    pub wireframe: Option<String>,
}

/// Lowered desktop window boot knobs — the unit `DesktopEnv.window` carries
/// and the chassis threads to the desktop driver and the bundle bins.
/// Mirrors the other embedded knob groups (`RingCapacities`,
/// `SchedulerTuning`, `RenderTuningConfig`) rather than riding as loose
/// fields. Produced by [`WindowConfig::lower`].
#[derive(Clone, Debug)]
pub struct WindowBoot {
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
    /// Lower the resolved window knobs into the [`WindowBoot`] unit the
    /// chassis threads into `DesktopEnv`. Subsumes the mode + title lowering:
    /// `mode` delegates to [`parse_window_mode_env`], warn-logging and
    /// falling back to `Windowed` on a bad value (preserving the
    /// pre-migration soft-fallback for `AETHER_WINDOW_MODE`); `title`
    /// maps `None` (unset or empty — the derive filters empty → `None`) to
    /// `"aether"` and passes a provided value through verbatim; `wireframe`
    /// rides through unchanged.
    #[must_use]
    pub fn lower(self) -> WindowBoot {
        let (mode, size) =
            self.mode.as_ref().map_or((WindowMode::Windowed, None), |s| match parse_window_mode_env(s) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::boot",
                        value = %s,
                        error = %e,
                        "AETHER_WINDOW_MODE unparseable — falling back to Windowed",
                    );
                    (WindowMode::Windowed, None)
                }
            });
        let title = self.title.unwrap_or_else(|| "aether".to_owned());

        WindowBoot { mode, size, title, wireframe: self.wireframe }
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
        assert_eq!(WindowConfig::default().lower().title, "aether");
    }

    #[test]
    fn lower_title_some_returns_value() {
        // Provided title passes through verbatim.
        let cfg = WindowConfig { mode: None, title: Some("my game".to_owned()), wireframe: None };
        assert_eq!(cfg.lower().title, "my game");
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
