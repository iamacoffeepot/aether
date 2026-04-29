// Mail envelope types. Owned by value because mails cross thread
// boundaries through the scheduler's queue.

use std::fmt;

use aether_hub_protocol::{EngineId, SessionToken};
use aether_mail::mailbox_id_from_name;
use aether_mail::tagged_id;

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

/// ADR-0064: render as the tagged string form (`mbx-XXXX-XXXX-XXXX`)
/// when a tracing call site uses `%`. Falls back to a hex dump for
/// reserved / invalid tag bits (`MailboxId::NONE` in particular) so a
/// stray sentinel doesn't silently render as a malformed prefixed
/// string.
impl fmt::Display for MailboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match tagged_id::encode(self.0) {
            Some(s) => f.write_str(&s),
            None => write!(f, "{:#018x}", self.0),
        }
    }
}

/// Host/guest contract tag for the payload layout. The substrate and the
/// components that talk to it agree on a specific layout per kind. The
/// typed facade over this is ADR-0005 (mail typing system) and ADR-0019
/// (schema-described kinds). Widened to `u64` in ADR-0030 Phase 1 in
/// preparation for hashed derivation (Phase 2).
pub type MailKind = u64;

/// Where a reply-bearing mail should route when the recipient
/// answers. Strictly a routing hint: mail is pushed at a recipient,
/// not sent from a mailbox.
///
/// `None` is the default — broadcast / substrate-generated mail
/// with no meaningful reply target. `Session` tags mail that
/// arrived from a Claude MCP session, so replies route back to
/// that session (ADR-0008). `EngineMailbox` tags mail bubbled up
/// from a component on another engine, so replies route to the
/// originating engine's mailbox (ADR-0037 Phase 2). `Component`
/// tags sink-bound mail that a local component pushed through
/// `SubstrateCtx::send`, so sink reply paths (ADR-0041's io sink is
/// the motivating case) can route the `*Result` back to the
/// component via the mailer rather than the hub.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReplyTarget {
    None,
    Session(SessionToken),
    EngineMailbox {
        engine_id: EngineId,
        mailbox_id: u64,
    },
    Component(MailboxId),
}

/// Reply-routing info for a `Mail` (ADR-0008, ADR-0037, ADR-0042).
/// The `target` describes where a reply goes; `correlation_id` is an
/// opaque u64 the original sender attached so it can identify its
/// specific request among replies of the same kind. The mailer
/// auto-echoes `correlation_id` when constructing a reply via
/// `send_reply`, so reply-bearing sinks (the io sink today) don't
/// need per-sink echo code. `0` means "no correlation"; waits with
/// `expected_correlation == 0` match any correlation (backward-compat
/// and for non-correlating callers like broadcasts and input mail).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplyTo {
    pub target: ReplyTarget,
    pub correlation_id: u64,
}

impl ReplyTo {
    /// Sentinel for no correlation. Using the explicit constant makes
    /// call sites self-documenting; a plain `0` in code comments as
    /// "why zero?" whereas `NO_CORRELATION` makes the intent obvious.
    pub const NO_CORRELATION: u64 = 0;

    /// `ReplyTo` with no target and no correlation.
    pub const NONE: ReplyTo = ReplyTo {
        target: ReplyTarget::None,
        correlation_id: Self::NO_CORRELATION,
    };

    /// Reply target alone, no correlation. Short form for mail paths
    /// that want to address a reply but don't participate in the
    /// ADR-0042 correlation scheme (the hub's inbound session mail
    /// today — a future change could have sessions carry correlation
    /// when the MCP send_mail tool grows to expose it).
    pub fn to(target: ReplyTarget) -> Self {
        Self {
            target,
            correlation_id: Self::NO_CORRELATION,
        }
    }

    /// Target + correlation. The common sync-wrapper shape.
    pub fn with_correlation(target: ReplyTarget, correlation_id: u64) -> Self {
        Self {
            target,
            correlation_id,
        }
    }

    /// Whether the reply target is `None`. Existing callers that
    /// were pattern-matching on the pre-refactor `ReplyTo::None`
    /// variant use this instead.
    pub fn is_none(&self) -> bool {
        matches!(self.target, ReplyTarget::None)
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
