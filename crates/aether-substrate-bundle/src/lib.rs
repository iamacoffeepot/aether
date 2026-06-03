//! aether-substrate-bundle: multi-binary chassis crate (ADR-0073).
//!
//! Standard Cargo layout:
//!
//! - `src/<chassis>/` — chassis-specific source (chassis impl,
//!   driver capability, render plumbing, etc.) for the four chassis:
//!   `desktop`, `headless`, `hub`, `test_bench`.
//! - `src/hub/` — the hub chassis (the `aether-substrate-hub` binary's
//!   thin Chassis impl post-issue-763 P5f).
//! - `src/test_bench/` — the test-bench chassis plus the in-process
//!   `TestBench` library API consumers reach via
//!   `aether_substrate_bundle::test_bench::TestBench`.
//! - `src/bin/<chassis>.rs` — minimal entry point per binary
//!   (`aether-substrate`, `aether-substrate-headless`,
//!   `aether-substrate-hub`, `aether-substrate-test-bench` —
//!   output names preserved across the rename).
//!
//! The lib root re-exports a convenience surface (the most-used
//! `aether-substrate` runtime types) so external consumers —
//! components, integration tests, the scenario runner, demos — can
//! write `use aether_substrate_bundle::{Registry, ...};` instead of
//! chasing through chassis submodules. The shared substrate runtime
//! (mail scheduler, registry, wasmtime host, capabilities) lives in
//! `aether-substrate` — depend on that directly when you don't need
//! chassis surface.

mod chassis_common;
pub use chassis_common::{PersistOverride, chassis_config_dump};
pub mod chassis_root;
pub mod cli;
pub mod desktop;
pub mod headless;
pub mod hub;
pub mod perf;
pub mod test_bench;

pub use aether_capabilities as capabilities;
pub use aether_capabilities::{ComponentHostCapability, ComponentHostConfig};
pub use aether_substrate::{
    Chassis, Component, ComponentCtx, HubOutbound, InboxHandler, InlineHandler, KindId, Mail,
    MailKind, MailboxEntry, MailboxId, Mailer, OwnedDispatch, Registry, ReplyTarget, ReplyTo,
    SubstrateBoot,
    actor::wasm::{component, host_fns, kind_manifest, reply_table},
    capture::{CaptureQueue, PendingCapture},
    chassis::frame_loop,
    mail,
    mail::mailer,
    mail::registry,
    runtime::log_install,
};
