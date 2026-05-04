// Mail envelope types. Owned by value because mails cross thread
// boundaries through the scheduler's queue.

/// Addressing token for any mailbox — component or substrate-owned sink.
/// ADR-0065 hoisted the canonical home into `aether_data` (per ADR-0069);
/// this remains re-exported under the `aether_substrate::mail::MailboxId`
/// path so existing call sites compile unchanged.
pub use aether_data::{KindId, MailboxId};
/// Reply-routing types — ADR-0075 / issue 533 PR D1 hoisted these into
/// `aether-data` so chassis caps in `aether-kinds` can name them from
/// `#[handler]` signatures. This module re-exports them so existing
/// `aether_substrate::mail::{ReplyTo, ReplyTarget}` call sites compile
/// unchanged.
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
/// Reply routing is carried in two complementary fields. Neither
/// describes where the mail came from; both describe where a reply
/// would go if the receiver chooses to make one.
/// - `reply_to` is the remote destination (ADR-0008 / ADR-0037).
///   `Session` means reply routes back to a Claude MCP session;
///   `EngineMailbox` means reply routes to a component on another
///   engine; `None` means no remote reply target.
/// - `from_component` is the `MailboxId` of a local originating
///   component for mail enqueued by `SubstrateCtx::send` (ADR-0017).
///   `Some` means reply routes back to that local mailbox.
///
/// Both-absent (`reply_to = None`, `from_component = None`) means
/// broadcast-origin or substrate-generated mail with no reply
/// target.
#[derive(Debug)]
pub struct Mail {
    pub recipient: MailboxId,
    pub kind: MailKind,
    pub payload: Vec<u8>,
    pub count: u32,
    pub reply_to: ReplyTo,
    pub from_component: Option<MailboxId>,
}

impl Mail {
    pub fn new(recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) -> Self {
        Self {
            recipient,
            kind,
            payload,
            count,
            reply_to: ReplyTo::NONE,
            from_component: None,
        }
    }

    /// Attach a reply-to destination (session or engine mailbox).
    /// Used by the hub client when forwarding inbound frames
    /// (ADR-0008) and by the hub-chassis loopback when delivering
    /// bubbled-up mail (ADR-0037 Phase 2); other mail paths leave
    /// the default `ReplyTo::None`.
    pub fn with_reply_to(mut self, reply_to: ReplyTo) -> Self {
        self.reply_to = reply_to;
        self
    }

    /// Attach the originating component's mailbox id. Set by
    /// `SubstrateCtx::send` when enqueueing component-to-component
    /// mail (ADR-0017) so `Component::deliver` can allocate a
    /// Component-variant reply handle for the receiving guest.
    pub fn with_origin(mut self, origin: MailboxId) -> Self {
        self.from_component = Some(origin);
        self
    }
}
