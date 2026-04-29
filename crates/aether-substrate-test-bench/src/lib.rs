//! aether-substrate-test-bench: the test-bench chassis binary crate (ADR-0067).
//!
//! Holds the wgpu renderer (no presentation surface), the
//! `CaptureQueue` handoff slot for synchronous capture, and the
//! chassis-side control-plane handler that owns capture_frame and
//! replies `Err` on the window-only kinds (set_window_mode,
//! set_window_title, platform_info). The shared runtime lives in
//! `aether-substrate-core`; this lib re-exports the subset the
//! binary and its dependents (the smoke runner per ADR-0067) lean
//! on.
//!
//! The lib + bin split means Rust integration tests can link the
//! chassis driver directly without going through process spawning.

pub mod capture;
pub mod chassis;
pub mod render;

pub use aether_substrate_core::{
    AETHER_CONTROL, Chassis, ChassisCapabilities, ChassisControlHandler, Component, ControlPlane,
    HUB_CLAUDE_BROADCAST, HubClient, HubOutbound, InputSubscribers, Mail, MailKind, MailboxEntry,
    MailboxId, Mailer, Registry, ReplyTarget, ReplyTo, Scheduler, SinkHandler, SubstrateBoot,
    SubstrateCtx, component, control, ctx, host_fns, hub_client, input, io, kind_manifest,
    log_capture, mail, mailer, new_subscribers, registry, remove_from_all, reply_table, scheduler,
    subscribers_for,
};

pub use capture::{CaptureQueue, PendingCapture};
pub use chassis::chassis_control_handler;
