//! aether-substrate-bundle: multi-binary chassis crate (ADR-0073).
//!
//! Standard Cargo layout:
//!
//! - `src/<chassis>/` — chassis-specific source (chassis impl,
//!   driver capability, render plumbing, etc.) for the desktop and
//!   headless chassis. The hub chassis lives in `aether-chassis-hub`.
//! - `src/bin/<chassis>.rs` — minimal entry point per binary
//!   (`aether-substrate`, `aether-substrate-headless`,
//!   `aether-substrate-harness`).
//!
//! The substrate-harness chassis machinery and the in-process
//! `SubstrateHarness` live in the `aether-harness-substrate` crate
//! (GPU capture support in `aether-harness-substrate-capture`); this
//! crate keeps the `aether-substrate-harness` binary entry point. The
//! shared chassis-composition layer — boot fragments, config registry,
//! CLI roots, autoload, the bundle-pack format — lives in
//! `aether-chassis` (issue #3809).
//!
//! The lib root re-exports a convenience surface (the most-used
//! `aether-substrate` runtime types) so external consumers —
//! components, integration tests, the scenario runner, demos — can
//! write `use aether_substrate_bundle::{Registry, ...};` instead of
//! chasing through chassis submodules. The shared substrate runtime
//! (mail scheduler, registry, wasmtime host, capabilities) lives in
//! `aether-substrate` — depend on that directly when you don't need
//! chassis surface.

pub mod desktop;
pub mod headless;

pub use aether_component::{ComponentHostCapability, ComponentHostConfig};
pub use aether_substrate::{
    Chassis, Component, ComponentCtx, HubOutbound, InboxHandler, InlineHandler, KindId, Mail, MailKind, MailboxEntry,
    MailboxId, Mailer, OwnedDispatch, Registry, RingCapacities, SchedulerTuning, Source, SourceAddr, SubstrateBoot,
    actor::wasm::{component, host_fns, kind_manifest, reply_table},
    capture::{CaptureQueue, PendingCapture},
    chassis::frame_loop,
    mail,
    mail::mailer,
    mail::registry,
    runtime::log_install,
};

#[cfg(test)]
mod chassis_source_guard {
    /// Regression guard for the enable / disable convention (#1791): a
    /// capability's enable/disable flag is resolved through its
    /// derive-`Config` (`*Config::from_argv_then_env`), never a raw
    /// `env::var` read in a chassis builder. This is the shape #1761 put
    /// the http server on; the guard keeps a future cap from regressing to
    /// presence-inference or a hand-rolled env read. The chassis window /
    /// tick / boot knobs are now also derive-`Config` (`WindowConfig`,
    /// `TickConfig`, `ChassisBootConfig`), so no raw `env::var` of any
    /// known `AETHER_*` key should appear in the chassis builder sources.
    #[test]
    fn chassis_builders_resolve_cap_enable_flags_via_config() {
        // Enable / disable env keys owned by a derive-`Config` cap. Add a
        // cap's flag key here when a new opt-in / opt-out cap lands.
        const CAP_FLAG_KEYS: &[&str] = &["AETHER_HTTP_SERVER_ENABLED", "AETHER_AUDIO_DISABLE"];
        let desktop = include_str!("desktop/chassis.rs");
        let headless = include_str!("headless/chassis.rs");
        for key in CAP_FLAG_KEYS {
            let raw_read = format!("env::var(\"{key}\")");
            for (chassis, src) in [("desktop", desktop), ("headless", headless)] {
                assert!(
                    !src.contains(&raw_read),
                    "{chassis} chassis reads {key} via raw env::var — route it through the \
                     cap's config API instead (see the `config` module's \
                     \"Enable / disable convention\")",
                );
            }
        }
    }
}
