//! aether-chassis: the shared chassis-composition layer (ADR-0073,
//! issue #3809).
//!
//! Every chassis binary composes the same substrate base — the common
//! cap set, the fleet-wide config registry, the argv/env resolution
//! stack, boot-time component autoload — and only the driver stack
//! (winit + wgpu, std timer, RPC coordinator, loopback harness)
//! differs. This crate owns that shared layer:
//!
//! - [`boot`] — the `Builder` boot fragments, the fleet-wide config
//!   registry behind `--print-config` / known-key sweeps, the
//!   chassis-wide boot knobs, and [`boot::boot_standard`], the shared
//!   `Chassis::build` body a full-stack chassis parameterises by its
//!   driver alone.
//! - [`cli`] — the per-chassis clap roots and per-cap overlay
//!   composition (ADR-0090 unit d), plus `chassis_cli!` — the root's
//!   `ChassisCli` impl and flag-parity test.
//! - [`entry`] — the shared chassis-binary `main` (`chassis_main!` /
//!   [`entry::run_chassis_main`]).
//! - [`autoload`] — boot-time component autoload shared by the
//!   full-stack chassis (issue #1529).
//! - [`boot_manifest`] — the JSON boot-manifest format the hub's
//!   `spawn_substrate` injection describes a component set with, plus the
//!   [`PackedComponent`](boot_manifest::PackedComponent) /
//!   [`ChassisSettings`](boot_manifest::ChassisSettings) autoload types the
//!   package depot ([`package`]) shares.
//! - [`WindowConfig`] / [`TickConfig`] — the desktop window and headless tick boot
//!   knobs, declared here because the fleet-wide registry and the CLI
//!   roots name their derived layers/overlays.
//!
//! The layer sits above the cap crates: they all depend on
//! `aether-substrate` for their runtime halves, so shared composition
//! cannot live in the substrate without a cycle.

use std::sync::atomic::{AtomicU64, Ordering};

/// Re-exported for `chassis_main!`, whose emitted `fn main` names this crate's
/// `anyhow` rather than requiring every chassis bin to carry a dependency it
/// would otherwise never spell.
pub use anyhow;

pub mod autoload;
pub mod boot;
pub mod boot_manifest;
pub mod cli;
pub mod entry;
pub mod package;
pub mod tick;
pub mod window;

pub use aether_substrate::chassis::{BuildProvenance, PreludeAction, PreludeFlags};
pub use boot::{
    boot_standard, build_provenance, chassis_residual_knobs, hub_residual_knobs, resolve_teardown_budget,
    run_describe_prelude,
};
pub use entry::{ChassisEnv, run_chassis_main};
pub use tick::{DEFAULT_TICK_HZ, TickConfig, TickConfigLayer, TickOverlay, apply_manifest_tick_settings};
pub use window::{
    WindowConfig, WindowConfigLayer, WindowOverlay, WindowSettings, apply_manifest_window_settings,
    parse_window_mode_env,
};

/// Atomically advance `counter` and return the next non-zero id for
/// synthetic chassis-root mail (ADR-0080 §6). Both the headless driver
/// and the substrate-harness bin own an `AtomicU64` of these;
/// symmetric with the per-actor counter on `NativeBinding`, with zero
/// reserved as the `MailId::NONE` sentinel.
pub fn next_chassis_correlation(counter: &AtomicU64) -> u64 {
    let id = counter.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        counter.fetch_add(1, Ordering::Relaxed)
    } else {
        id
    }
}
