//! The `aether.tcp` cap runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpCapability`](super::TcpCapability) identity never names these types
//! nor pulls `aether_substrate`. The substrate / `std::net`-typed imports are
//! gated once by this module rather than line-by-line; the `#[actor] impl`
//! reaches the state, ctx types, and supervisor structs through the single
//! `use runtime::*` glob in the parent.

pub use std::collections::HashMap;
pub use std::net::TcpListener;

pub use aether_actor::Manual;
// The manual handlers (`on_unbind` / `on_monitor_notice`) issue their own
// replies through `ctx.reply` / `ctx.reply_to`, the `OutboundReply` trait
// methods, so the trait must be in scope where those handler bodies expand.
pub use aether_actor::OutboundReply;
pub use aether_substrate::actor::monitor::MonitorHandle;
pub use aether_substrate::actor::native::spawn::Subname;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// `aether.tcp` runtime state (issue 607 Phase 6a, ADR-0079). The singleton
/// control-plane cap owns its listener fleet directly — it is the supervisor,
/// not a thin shim over the chassis registry. Each `on_bind` registers a
/// monitor on the new listener and inserts a [`ListenerEntry`] into
/// `listeners`; `on_monitor_notice` removes the entry on listener close.
/// The addressing identity is the distinct ZST
/// [`TcpCapability`](super::TcpCapability). Living in this private module keeps
/// it `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
///
/// Issue 629 / Phase B: plain `HashMap` fields. The dispatcher thread is the
/// sole writer / reader; pre-Phase-A's `Mutex<HashMap<...>>` was a
/// worker-pool-era tax, not a contention point.
pub struct TcpCapabilityState {
    /// Live listeners spawned by this cap. Key is the listener's
    /// full-name `MailboxId`. Each entry holds the bind metadata
    /// surfaced via `ListListeners` plus the monitor handle that
    /// pins the cap's monitor on the listener until close.
    pub(super) listeners: HashMap<aether_data::MailboxId, ListenerEntry>,
    /// Outstanding unbind replies parked until `MonitorNotice`
    /// arrives from the listener being closed. Key is the same
    /// `MailboxId` as `listeners`; the cap's monitor (registered
    /// at spawn time) is what fires the notice.
    pub(super) pending_unbinds: HashMap<aether_data::MailboxId, PendingUnbind>,
}

/// Cap-local supervisor state for one live listener. Drops with
/// the entry; `MonitorHandle::Drop` is idempotent with the close
/// path's index drain.
pub(super) struct ListenerEntry {
    pub(super) addr: String,
    pub(super) port: u16,
    pub(super) name: String,
    // Held to keep the cap's monitor registered against the
    // listener for its lifetime. Drops when the entry is removed
    // (in `on_monitor_notice`).
    pub(super) _monitor_handle: MonitorHandle,
}

pub(super) struct PendingUnbind {
    pub(super) sender: aether_data::Source,
    pub(super) listener_name: String,
}
