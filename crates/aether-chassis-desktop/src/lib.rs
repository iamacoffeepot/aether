//! aether-chassis-desktop: the desktop chassis (ADR-0035/ADR-0073,
//! issue #3812) — winit event loop, wgpu renderer, capture queue, cpal
//! audio. Produces the `aether-substrate` binary over the shared
//! `aether-chassis` composition layer.
//! Issue 603 retired the chassis-side control-plane handler that
//! pre-Phases-2-4 owned `capture_frame` / window kinds /
//! `platform_info` — each kind now has its own cap (or, for
//! `platform_info`, was deleted entirely).

pub mod chassis;
pub mod driver;
pub mod render;

pub use chassis::{DesktopChassis, DesktopEnv, UserEvent};
pub use driver::{DesktopDriverCapability, DesktopDriverRunning};

pub use aether_chassis::autoload::AutoloadComponent;

#[cfg(test)]
mod chassis_source_guard {
    /// Regression guard for the enable / disable convention (#1791),
    /// desktop half — the headless half lives with the headless chassis
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
                "desktop chassis reads {key} via raw env::var — route it through the cap's config API instead",
            );
        }
    }
}
