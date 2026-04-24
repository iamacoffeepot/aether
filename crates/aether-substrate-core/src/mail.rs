// Mail envelope types. Owned by value because mails cross thread
// boundaries through the scheduler's queue.

use aether_hub_protocol::{EngineId, SessionToken};
use aether_mail::mailbox_id_from_name;

/// Addressing token for any mailbox — component or substrate-owned sink.
/// Opaque `u64` newtype so it can't be accidentally mixed with wasmtime
/// indices or raw integers. The id is `aether_mail::mailbox_id_from_name`
/// of the mailbox's registered name (ADR-0029) — deterministic across
/// processes and sessions.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailboxId(pub u64);

impl MailboxId {
    /// Reserved sentinel for "no origin". Registration rejects any
    /// name whose hash collides with 0 (practical probability
    /// ~2⁻⁶⁴, but the guard is cheap) so this id never belongs to a
    /// real mailbox.
    pub const NONE: MailboxId = MailboxId(0);

    /// Compute the deterministic id for a mailbox name. Same algorithm
    /// the guest SDK uses on the component side — ids round-trip
    /// verbatim across the FFI.
    pub fn from_name(name: &str) -> MailboxId {
        MailboxId(mailbox_id_from_name(name))
    }
}

/// Host/guest contract tag for the payload layout. The substrate and the
/// components that talk to it agree on a specific layout per kind. The
/// typed facade over this is ADR-0005 (mail typing system) and ADR-0019
/// (schema-described kinds). Widened to `u64` in ADR-0030 Phase 1 in
/// preparation for hashed derivation (Phase 2).
pub type MailKind = u64;

/// Reply destination for a `Mail` (ADR-0008, ADR-0037). Strictly a
/// reply-to hint: mail is pushed at a recipient, not sent from a
/// mailbox, so there is no actual "sender" concept in the system —
/// this field records where an optional reply should be routed.
///
/// `None` is the default — broadcast / substrate-generated mail
/// with no meaningful reply target. `Session` tags mail that
/// arrived from a Claude MCP session, so replies route back to
/// that session (ADR-0008). `EngineMailbox` tags mail bubbled up
/// from a component on another engine, so replies route to the
/// originating engine's mailbox (ADR-0037 Phase 2). The hub-
/// chassis fills in the engine id on `EngineMailToHubSubstrate`
/// reception from the TCP connection it arrived on.
/// `Component` tags sink-bound mail that a local component
/// pushed through `SubstrateCtx::send`, carrying the sender's
/// own mailbox so sink reply paths (ADR-0041's io sink is the
/// motivating case) can route the `*Result` back to the component
/// via the mailer rather than the hub.
///
/// `Mail.from_component` carries the same information for mail
/// the mailer routed into a recipient's inbox (so
/// `Component::deliver` can allocate a Component-variant reply
/// handle), but sink dispatch skips the `Mail` struct entirely —
/// the handler is called inline. For that path the reply target
/// rides on this enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReplyTo {
    None,
    Session(SessionToken),
    EngineMailbox {
        engine_id: EngineId,
        mailbox_id: u64,
    },
    Component(MailboxId),
}

impl ReplyTo {
    /// Whether the variant carries no reply target. Callers deciding
    /// whether to allocate a reply-handle table entry use this
    /// before pattern matching on the payload.
    pub fn is_none(&self) -> bool {
        matches!(self, ReplyTo::None)
    }
}

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
            reply_to: ReplyTo::None,
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
