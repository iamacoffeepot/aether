//! aether-substrate-desktop: the desktop chassis binary crate.
//!
//! Holds the winit event loop, wgpu renderer, the `CaptureQueue`
//! handoff slot for async capture, and the chassis-side control-plane
//! handler (`chassis::chassis_control_handler`) that registers the
//! desktop-only kinds (capture_frame, set_window_mode, platform_info)
//! against core's `ControlPlane` fallback. The shared runtime lives
//! in `aether-substrate-core`; this lib is a convenience re-export
//! hub for the binary's own modules plus the core surface they lean
//! on. See ADR-0035.
//!
//! The built binary is still named `aether-substrate` (see the
//! `[[bin]]` override in Cargo.toml) so `spawn_substrate` paths and
//! existing tool scripts keep resolving it.

pub mod audio;
pub mod capture;
pub mod chassis;

pub use aether_substrate_core::{
    AETHER_CONTROL, Chassis, ChassisCapabilities, ChassisControlHandler, Component, ControlPlane,
    HUB_CLAUDE_BROADCAST, HubClient, HubOutbound, InputSubscribers, Mail, MailKind, MailboxEntry,
    MailboxId, Mailer, Registry, ReplyTo, Scheduler, SinkHandler, SubstrateBoot, SubstrateCtx,
    component, control, ctx, host_fns, hub_client, input, kind_manifest, log_capture, mail, mailer,
    new_subscribers, registry, remove_from_all, reply_table, scheduler, subscribers_for,
};

pub use capture::{CaptureQueue, PendingCapture};
pub use chassis::{UserEvent, chassis_control_handler};
