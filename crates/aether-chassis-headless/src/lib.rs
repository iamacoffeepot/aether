//! aether-chassis-headless: the headless chassis (ADR-0035/ADR-0073,
//! issue #3811). Std-timer driven, no GPU, no window; replies `Err` to
//! capture / window-mode kinds — desktop-only operations this
//! deployment doesn't support. Produces the `aether-substrate-headless`
//! binary over the shared `aether-chassis` composition layer.

pub mod chassis;
pub mod driver;

pub use chassis::{HeadlessChassis, HeadlessEnv};

pub use aether_chassis::autoload::AutoloadComponent;

#[cfg(test)]
mod chassis_source_guard {
    /// Regression guard for the enable / disable convention (#1791),
    /// headless half — the desktop half lives with the desktop chassis
    /// sources. A capability's enable/disable flag resolves through its
    /// derive-`Config`, never a raw `env::var` read in a chassis builder.
    #[test]
    fn chassis_builder_resolves_cap_enable_flags_via_config() {
        const CAP_FLAG_KEYS: &[&str] = &["AETHER_HTTP_SERVER_ENABLED", "AETHER_AUDIO_DISABLE"];
        let src = include_str!("chassis.rs");
        for key in CAP_FLAG_KEYS {
            let raw_read = format!("env::var(\"{key}\")");
            assert!(
                !src.contains(&raw_read),
                "headless chassis reads {key} via raw env::var — route it through the cap's config API instead",
            );
        }
    }
}
