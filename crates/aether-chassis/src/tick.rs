//! Headless tick-cadence boot knob. Declared here (not in the headless
//! chassis) because the fleet-wide config registry in [`crate::boot`]
//! and the shared CLI roots in [`crate::cli`] both name the derived
//! `TickConfigLayer` / `TickOverlay` — the hub dumps the full fleet key
//! set since a hub-spawned substrate inherits the hub's env.

use std::time::Duration;

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
    /// `AETHER_TICK_HZ=<hz>` headless tick cadence in hertz (default 60).
    /// `nonzero` maps `0` to the default so the timer always gets a valid
    /// period; a garbage string hard-errors at boot.
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

#[cfg(test)]
mod tests {
    use super::{DEFAULT_TICK_HZ, TickConfig, TickConfigLayer};
    use confique::Config as _;
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
}
