//! Headless tick-cadence boot knob. Declared here (not in the headless
//! chassis) because the fleet-wide config registry in [`crate::boot`]
//! and the shared CLI roots in [`crate::cli`] both name the derived
//! `TickConfigLayer` / `TickOverlay` — the hub dumps the full fleet key
//! set since a hub-spawned substrate inherits the hub's env.

use std::time::Duration;

use aether_substrate::config::{ConfigError, ConfigProvenance, ConfigSources};

use crate::boot_manifest::ChassisSettings;

pub const DEFAULT_TICK_HZ: u32 = 60;

/// Headless tick-cadence knob (ADR-0090 §1/§2 applied to the chassis's
/// own tick knob). The `#[derive(aether_substrate::Config)]` emits the
/// env-shaped `TickConfigLayer`, the clap-shaped `TickOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` / `try_*` shims — mirrors
/// `ActorRingConfig` in [`crate::boot`].
///
/// `env_prefix = "AETHER_TICK"` + field `hz` → `AETHER_TICK_HZ`, matching
/// the historical key. The `nonzero` hint maps a resolved `0` to the
/// default (60), reproducing the old `parse_tick_hz_env` `>0` filter. A
/// garbage value hard-errors at boot (ADR-0090 §4 strict path) instead of
/// the old soft-warn fallback.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_TICK", cli_prefix = "tick")]
pub struct TickConfig {
    /// Headless tick rate in hertz.
    ///
    /// Default 60. `nonzero` maps `0` to the default so the timer always
    /// gets a valid period; a garbage string hard-errors at boot.
    #[config(default = 60, nonzero)]
    pub hz: u32,
}

impl Default for TickConfig {
    fn default() -> Self {
        Self { hz: DEFAULT_TICK_HZ }
    }
}

impl TickConfig {
    /// Lower to the tick [`Duration`] the chassis timer uses.
    #[must_use]
    pub fn to_tick_period(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.hz))
    }
}

/// Overlay a depot package manifest's tick cadence onto `sources` BELOW
/// argv/env/file, ABOVE the compiled default (issue 4001) — the headless
/// half of applying a `--package` / `AETHER_PACKAGE` manifest's
/// [`ChassisSettings`].
///
/// The standalone-bundle bin overlays `tick_hz` as a top-priority
/// `set_override` (the binary *is* the product, ADR-0163 §1); on the shared
/// headless chassis an operator must keep the ability to override a shipped
/// package with `AETHER_TICK_HZ` / `--tick-hz`, so the manifest slots in one
/// layer lower. The mechanism: ask the stack which layer supplies
/// [`TickConfig`], and only when that is
/// [`ConfigProvenance::Default`] — no argv, env, file, or programmatic source
/// carried a cadence — substitute the manifest's value, re-staging the result
/// as the programmatic override the headless `Chassis::build` resolves.
///
/// The provenance read is what makes an explicit pin of the compiled default
/// honour precedence (issue 4006). `TickConfig.hz` is a plain `u32` with no
/// unset sentinel, so the older shape detected "unset" by comparing the
/// *resolved* value against [`DEFAULT_TICK_HZ`] — which made an operator's
/// `AETHER_TICK_HZ=60` over a manifest carrying a different cadence silently
/// lose to the manifest, the one case where env did not beat it. Asking which
/// layer won answers the question the fold cannot.
///
/// A `title` / `window_mode` in the manifest is a desktop knob the headless
/// chassis has no window for, so it is warn-ignored here (mirroring the bundle
/// bins).
///
/// # Errors
///
/// Propagates the [`ConfigError`] from resolving [`TickConfig`] off the stack
/// when a known `AETHER_TICK_*` value is malformed (ADR-0090 §4).
pub fn apply_manifest_tick_settings(
    sources: &mut ConfigSources,
    settings: &ChassisSettings,
) -> Result<(), ConfigError> {
    if settings.title.is_some() || settings.window_mode.is_some() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "depot package sets title/window_mode, which the headless chassis ignores (no window)",
        );
    }
    // A `0` cadence is the unset sentinel (`nonzero` maps it to the default), so
    // treat a manifest `Some(0)` as "no cadence carried" and leave the stack's
    // own resolution untouched.
    let Some(manifest_hz) = settings.tick_hz.filter(|hz| *hz > 0) else {
        return Ok(());
    };

    // Read the provenance before resolving: resolution consumes the staged
    // argv layer and programmatic override, so asking afterwards would report
    // `Default` for a cadence those layers supplied.
    let supplied = sources.provenance_of::<TickConfig>() != ConfigProvenance::Default;
    let resolved = sources.resolve::<TickConfig>()?;
    let hz = if supplied {
        resolved.hz
    } else {
        manifest_hz
    };
    sources.set_override(TickConfig { hz });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ChassisSettings, ConfigSources, DEFAULT_TICK_HZ, TickConfig, TickConfigLayer, apply_manifest_tick_settings,
    };
    use confique::Config as _;
    use std::env;
    use std::sync::Mutex;
    use std::time::Duration;

    #[test]
    fn tick_config_defaults_match() {
        // No `.env()` source: literal defaults only — env-free. The
        // `default = 60` literal must equal `DEFAULT_TICK_HZ` so an
        // unset knob reproduces the const default.
        // Tripwire: drifts when `DEFAULT_TICK_HZ` or the derive literal changes.
        let layer = TickConfigLayer::builder().load().expect("defaults load");
        assert_eq!(layer.hz, DEFAULT_TICK_HZ, "derive default must match DEFAULT_TICK_HZ");
        assert_eq!(TickConfig::default().hz, DEFAULT_TICK_HZ);
    }

    #[test]
    fn to_tick_period_maps_hz_to_duration() {
        // The only lowering logic this crate owns: hz → Duration.
        assert_eq!(TickConfig { hz: 60 }.to_tick_period(), Duration::from_nanos(1_000_000_000 / 60),);
        assert_eq!(TickConfig { hz: 120 }.to_tick_period(), Duration::from_nanos(1_000_000_000 / 120),);
    }

    /// Serializes the env-mutating tests in this module (the process
    /// environment is global): every test that sets `AETHER_TICK_HZ` holds
    /// this so a concurrent tick resolve never observes a half-set key.
    static TICK_ENV_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn apply_manifest_tick_beats_default() {
        // Manifest beats default (issue 4001): with no higher source supplying
        // the cadence, a depot package manifest's tick_hz reaches the resolved
        // `TickConfig`. Hermetic sources so the resolve reads no env — the "no
        // higher source" case is deterministic.
        let mut sources = ConfigSources::hermetic();
        let settings = ChassisSettings { title: None, window_mode: None, tick_hz: Some(30) };
        apply_manifest_tick_settings(&mut sources, &settings).expect("apply tick settings");
        let resolved = sources.resolve::<TickConfig>().expect("resolve tick config");
        assert_eq!(resolved.hz, 30, "manifest tick_hz fills the default cadence");
    }

    #[test]
    fn apply_manifest_tick_yields_to_env() {
        // Env beats manifest (issue 4001 precedence): an operator's
        // `AETHER_TICK_HZ` overrides a shipped package's tick_hz, so the shared
        // headless binary stays tunable. The bug this catches is the manifest
        // overlaying at top priority (like the bundle bins) and shadowing the
        // operator's env override. `120` differs from the compiled default so
        // the env value is distinguishable from an unset knob.
        let _guard = TICK_ENV_GUARD.lock().expect("env guard");
        // SAFETY: the guard serializes every env-touching test in this module,
        // and the key is removed before the guard drops.
        unsafe { env::set_var("AETHER_TICK_HZ", "120") };
        let mut sources = ConfigSources::new(None);
        let settings = ChassisSettings { title: None, window_mode: None, tick_hz: Some(30) };
        let resolved =
            apply_manifest_tick_settings(&mut sources, &settings).and_then(|()| sources.resolve::<TickConfig>());
        // SAFETY: same guarded scope.
        unsafe { env::remove_var("AETHER_TICK_HZ") };
        assert_eq!(resolved.expect("apply + resolve").hz, 120, "env AETHER_TICK_HZ overrides the manifest tick_hz");
    }

    #[test]
    fn apply_manifest_tick_yields_to_an_env_pin_of_the_compiled_default() {
        // The #4006 case: an operator pins AETHER_TICK_HZ to the compiled
        // default over a manifest carrying a different cadence. `hz` is a plain
        // u32 with no unset sentinel, so the old resolved-value-versus-default
        // comparison could not tell this explicit pin from an unset knob and
        // handed the manifest the win — the one case where env lost. The
        // provenance read distinguishes them, so 60 must survive here even
        // though 60 *is* DEFAULT_TICK_HZ.
        let _guard = TICK_ENV_GUARD.lock().expect("env guard");
        // SAFETY: the guard serializes every env-touching test in this module,
        // and the key is removed before the guard drops.
        unsafe { env::set_var("AETHER_TICK_HZ", DEFAULT_TICK_HZ.to_string()) };
        let mut sources = ConfigSources::new(None);
        let settings = ChassisSettings { title: None, window_mode: None, tick_hz: Some(30) };
        let resolved =
            apply_manifest_tick_settings(&mut sources, &settings).and_then(|()| sources.resolve::<TickConfig>());
        // SAFETY: same guarded scope.
        unsafe { env::remove_var("AETHER_TICK_HZ") };
        assert_eq!(
            resolved.expect("apply + resolve").hz,
            DEFAULT_TICK_HZ,
            "an explicit env pin of the compiled default still beats the manifest tick_hz"
        );
    }
}
