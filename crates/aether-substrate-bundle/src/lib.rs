//! aether-substrate-bundle: multi-binary chassis crate (ADR-0073).
//!
//! Standard Cargo layout:
//!
//! - `src/<chassis>/` — chassis-specific source (chassis impl,
//!   driver capability, render plumbing, etc.) for the four chassis:
//!   `desktop`, `headless`, `hub`, `test_bench`.
//! - `src/hub/` — the hub library (substrate-side client, wire types,
//!   MCP coordinator, hub chassis).
//! - `src/test_bench/` — the test-bench chassis plus the in-process
//!   `TestBench` library API consumers reach via
//!   `aether_substrate_bundle::test_bench::TestBench`.
//! - `src/bin/<chassis>.rs` — minimal entry point per binary
//!   (`aether-substrate`, `aether-substrate-headless`,
//!   `aether-substrate-hub`, `aether-substrate-test-bench` —
//!   output names preserved across the rename).
//!
//! The lib root re-exports a convenience surface (the hub types and
//! the most-used `aether-substrate` runtime types) so external
//! consumers — components, integration tests, the scenario runner,
//! demos — can write `use aether_substrate_bundle::{HubClient,
//! Registry, ...};` instead of chasing through chassis submodules.
//! The shared substrate runtime (mail scheduler, registry, wasmtime
//! host, capabilities) lives in `aether-substrate` — depend on that
//! directly when you don't need chassis or hub surface.

pub mod desktop;
pub mod headless;
pub mod hub;
pub mod test_bench;

pub use aether_capabilities as capabilities;
pub use aether_capabilities::BroadcastCapability;
pub use aether_substrate::{
    AETHER_CONTROL, Chassis, ChassisControlHandler, Component, ControlPlane, HubOutbound,
    InputSubscribers, KindId, Mail, MailKind, MailboxEntry, MailboxId, Mailer, Registry,
    ReplyTarget, ReplyTo, Scheduler, SinkHandler, SubstrateBoot, SubstrateCtx,
    capture::{CaptureQueue, PendingCapture},
    component, control, ctx, frame_loop, host_fns, input, kind_manifest, log_capture, mail, mailer,
    new_subscribers, registry, remove_from_all, reply_table, scheduler, subscribers_for,
};
pub use hub::{HubClient, dispatch_hub_mail_by_id, dispatch_hub_to_engine_mail};
