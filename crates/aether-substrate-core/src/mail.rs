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
    /// Reserved sentinel for "no sender". ADR-0011 / ADR-0017 treat
    /// `MailboxId(0)` as the unassigned origin; registration rejects
    /// any name whose hash collides with 0 (practical probability
    /// ~2⁻⁶⁴, but the guard is cheap).
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

/// Attribution of the remote-origin side of a `Mail` (ADR-0008,
/// ADR-0037). `None` is the default — broadcast / substrate-generated
/// mail with no meaningful reply-over-the-hub-wire target. `Session`
/// tags mail that arrived from a Claude MCP session (ADR-0008).
/// `EngineMailbox` tags mail bubbled up from a component on another
/// engine (ADR-0037 Phase 2) — the hub-chassis fills this in on the
/// receiving side of `EngineMailToHubSubstrate` frames so the
/// `ctx.reply` round trip can route back to the originating engine.
///
/// Note: local component-to-component attribution lives in
/// `Mail.from_component`, not here — it's pure substrate-local and
/// needs no hub-wire story. The two fields are complementary, not
/// redundant (broadcast mail from a local sink arrives with
/// `sender = None` and `from_component = None`; a session mail with
/// a typo-routed recipient arrives with `sender = Session(token)`
/// and `from_component = None`; a replied bubble arrives with
/// `sender = EngineMailbox {..}` and `from_component = None`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Sender {
    None,
    Session(SessionToken),
    EngineMailbox {
        engine_id: EngineId,
        mailbox_id: u64,
    },
}

impl Sender {
    /// Back-compat helper: true when the variant carries no reply
    /// target (replaces the old `sender == SessionToken::NIL`
    /// check). Callers deciding whether to allocate a sender table
    /// handle use this before pattern matching on the payload.
    pub fn is_none(&self) -> bool {
        matches!(self, Sender::None)
    }
}

/// The transport envelope. `payload` is the exact byte layout the kind
/// implies; `count` is the number of items the layout implies, where
/// applicable.
///
/// Origin attribution is carried in two complementary fields:
/// - `sender` is the remote origin (ADR-0008 / ADR-0037). `Session`
///   means the mail arrived from a Claude MCP session;
///   `EngineMailbox` means it bubbled up from a component on another
///   engine; `None` means no remote reply target.
/// - `from_component` is the `MailboxId` of a local originating
///   component for mail enqueued by `SubstrateCtx::send` (ADR-0017).
///   `Some` means "originated from another component on this
///   substrate."
///
/// Both-absent (`sender = None`, `from_component = None`) means
/// broadcast-origin or substrate-generated mail with no meaningful
/// reply target.
#[derive(Debug)]
pub struct Mail {
    pub recipient: MailboxId,
    pub kind: MailKind,
    pub payload: Vec<u8>,
    pub count: u32,
    pub sender: Sender,
    pub from_component: Option<MailboxId>,
}

impl Mail {
    pub fn new(recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) -> Self {
        Self {
            recipient,
            kind,
            payload,
            count,
            sender: Sender::None,
            from_component: None,
        }
    }

    /// Attach a sender attribution (session or engine mailbox). Used
    /// by the hub client when forwarding inbound frames (ADR-0008)
    /// and by the hub-chassis loopback when delivering bubbled-up
    /// mail (ADR-0037 Phase 2); other mail paths leave the default
    /// `Sender::None`.
    pub fn with_sender(mut self, sender: Sender) -> Self {
        self.sender = sender;
        self
    }

    /// Attach the originating component's mailbox id. Set by
    /// `SubstrateCtx::send` when enqueueing component-to-component
    /// mail (ADR-0017) so `Component::deliver` can allocate a
    /// `SenderEntry::Component` handle for the receiving guest.
    pub fn with_origin(mut self, origin: MailboxId) -> Self {
        self.from_component = Some(origin);
        self
    }
}
