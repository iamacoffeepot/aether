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
pub use outbound::{DroppingBackend, EgressBackend, EgressEvent, HubOutbound, RecordingBackend};
pub use registry::{InboxHandler, InlineHandler, MailboxEntry, OwnedDispatch, Registry};

/// Addressing token for any mailbox — component or substrate-owned sink.
/// ADR-0065 hoisted the canonical home into `aether_data` (per ADR-0069);
/// this remains re-exported under the `aether_substrate::mail::MailboxId`
/// path so existing call sites compile unchanged.
pub use aether_data::{KindId, MailId, MailboxId};
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
///   (set by `ComponentCtx::send` / `NativeBinding::send_mail`).
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
    /// ADR-0080 §1: this mail's identity. The producer mints it from
    /// `MailId::new(producer_mailbox, producer_per_actor_correlation)`
    /// before pushing through `Mailer`. PR 2 stamps it inert (no
    /// trace-event consumer reads it yet); PR 2's TraceObserver hooks
    /// emit `TraceEvent::Sent { mail_id, .. }` against this value.
    /// `MailId::NONE` for legacy paths that haven't migrated.
    pub mail_id: MailId,
    /// ADR-0080 §5: the root of this mail's causal chain — the
    /// originating mail's `mail_id` for the chain. Inherited from the
    /// sender's in-flight handler context; for chassis-root sends
    /// (`Tick`, lifecycle, externally-bridged), `root == mail_id`.
    /// `MailId::NONE` for legacy paths.
    pub root: MailId,
    /// ADR-0080 §5: the in-flight mail at the sender, or `None` for
    /// chassis-root sends. The receiver's parent in the causal graph.
    pub parent_mail: Option<MailId>,
}

impl Mail {
    pub fn new(recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) -> Self {
        Self {
            recipient,
            kind,
            payload,
            count,
            reply_to: ReplyTo::NONE,
            mail_id: MailId::NONE,
            root: MailId::NONE,
            parent_mail: None,
        }
    }

    /// Attach a reply-to destination. Used by the hub client when
    /// forwarding inbound frames (ADR-0008), the hub-chassis loopback
    /// when delivering bubbled-up mail (ADR-0037 Phase 2), and
    /// `ComponentCtx::send` / `NativeBinding::send_mail` for
    /// peer-to-peer component sends (target = `Component(sender)`).
    /// Other mail paths leave the default `ReplyTo::None`.
    pub fn with_reply_to(mut self, reply_to: ReplyTo) -> Self {
        self.reply_to = reply_to;
        self
    }

    /// ADR-0080 §1 / §5: stamp the producer-minted lineage triple
    /// (`mail_id`, `root`, `parent_mail`) onto this mail. Producer
    /// paths (`NativeBinding::send_mail`, `Mailer::send_reply`,
    /// chassis-root push sites) call this immediately after minting.
    /// Mail with no lineage stamped retains `MailId::NONE` defaults.
    pub fn with_lineage(
        mut self,
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
    ) -> Self {
        self.mail_id = mail_id;
        self.root = root;
        self.parent_mail = parent_mail;
        self
    }
}
