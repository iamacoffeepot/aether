//! Mail-routing primitives: the `Mail` envelope shape, the routing
//! [`Registry`], the [`Mailer`] that owns dispatch, and the
//! [`HubOutbound`] facade for cross-substrate egress.
//!
//! The mail layer is byte-transparent — all of these primitives operate
//! on raw payloads keyed by [`MailboxId`] and [`KindId`]. Typed
//! interaction lives in the actor SDK (`aether_actor::Mailbox<K>`) and
//! per-cap dispatchers.

pub mod helpers;
pub mod mailer;
pub mod outbound;
pub mod registry;

pub use mailer::Mailer;
pub use outbound::{
    DroppingBackend, EgressBackend, EgressEvent, HubOutbound, LogEntry, LogLevel, RecordingBackend,
};
pub use registry::{MailboxEntry, MailboxHandler, Registry};

/// Addressing token for any mailbox — component or substrate-owned sink.
/// ADR-0065 hoisted the canonical home into `aether_data` (per ADR-0069);
/// this remains re-exported under the `aether_substrate::mail::MailboxId`
/// path so existing call sites compile unchanged.
pub use aether_data::{KindId, MailboxId};
/// Reply-routing types. Canonical home is `aether-data` (ADR-0076) —
/// `aether-actor`'s `Dispatch` trait references them in its signature
/// without taking an `aether-actor`-internal dep, and the location
/// avoids a name clash with `aether-actor`'s wasm-side `ReplyTo` (a
/// distinct `u32` FFI handle). This module re-exports them so
/// existing `aether_substrate::mail::{ReplyTo, ReplyTarget}` call
/// sites compile unchanged.
pub use aether_data::{ReplyTarget, ReplyTo};
/// Host/guest contract tag for the payload layout. The substrate and the
/// components that talk to it agree on a specific layout per kind. The
/// typed facade over this is ADR-0005 (mail typing system) and ADR-0019
/// (schema-described kinds). Widened to `u64` in ADR-0030 Phase 1 in
/// preparation for hashed derivation (Phase 2); narrowed to the typed
/// `KindId` newtype in issue 459 to disambiguate kind ids by bit
/// pattern (`#[repr(transparent)]` over `u64`, so the wire shape is
/// unchanged).
pub type MailKind = KindId;

/// The transport envelope. `payload` is the exact byte layout the kind
/// implies; `count` is the number of items the layout implies, where
/// applicable.
///
/// `reply_to` describes where a reply would go if the receiver chooses
/// to make one (ADR-0008 / ADR-0037 / ADR-0017). It is *not* a "from"
/// field — it is the structural reply destination, which happens to
/// double as "who sent me this" when the variant is `Component`.
/// - `Session` — reply routes back to a Claude MCP session.
/// - `EngineMailbox` — reply routes to a component on another engine.
/// - `Component` — reply routes back to a local peer component
///   (set by `ComponentCtx::send` / `NativeTransport::send_mail`).
/// - `None` — no reply target (broadcast-origin or substrate-
///   generated mail).
///
/// Pre-issue-#644 a redundant `from_component: Option<MailboxId>`
/// also rode here, set by `with_origin` to the same id
/// `reply_to.target = Component(_)` already carried.
#[derive(Debug)]
pub struct Mail {
    pub recipient: MailboxId,
    pub kind: MailKind,
    pub payload: Vec<u8>,
    pub count: u32,
    pub reply_to: ReplyTo,
}

impl Mail {
    pub fn new(recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) -> Self {
        Self {
            recipient,
            kind,
            payload,
            count,
            reply_to: ReplyTo::NONE,
        }
    }

    /// Attach a reply-to destination. Used by the hub client when
    /// forwarding inbound frames (ADR-0008), the hub-chassis loopback
    /// when delivering bubbled-up mail (ADR-0037 Phase 2), and
    /// `ComponentCtx::send` / `NativeTransport::send_mail` for
    /// peer-to-peer component sends (target = `Component(sender)`).
    /// Other mail paths leave the default `ReplyTo::None`.
    pub fn with_reply_to(mut self, reply_to: ReplyTo) -> Self {
        self.reply_to = reply_to;
        self
    }
}
