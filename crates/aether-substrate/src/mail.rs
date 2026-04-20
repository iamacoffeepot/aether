// Mail envelope types. Owned by value because mails cross thread
// boundaries through the scheduler's queue.

use aether_hub_protocol::SessionToken;

/// Addressing token for any mailbox — component or substrate-owned sink.
/// Opaque `u64` newtype so it can't be accidentally mixed with wasmtime
/// indices or raw integers. Width is sized for the ADR-0029 move to
/// name-derived ids; today the registry still allocates sequentially.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailboxId(pub u64);

/// Host/guest contract tag for the payload layout. The substrate and the
/// components that talk to it agree on a specific layout per kind. The
/// typed facade over this is ADR-0005 (mail typing system) and ADR-0019
/// (schema-described kinds).
pub type MailKind = u32;

/// The transport envelope. `payload` is the exact byte layout the kind
/// implies; `count` is the number of items the layout implies, where
/// applicable.
///
/// Origin attribution is carried in two mutually-exclusive fields:
/// - `sender` is the hub-minted session token for mail that came in
///   over the hub wire (ADR-0008). Non-NIL means "originated from a
///   Claude session."
/// - `from_component` is the `MailboxId` of the originating component
///   for mail enqueued by `SubstrateCtx::send` (ADR-0017). `Some`
///   means "originated from another component on this substrate."
///
/// Both being absent (NIL + None) means broadcast-origin or
/// system-generated mail with no meaningful reply target.
#[derive(Debug)]
pub struct Mail {
    pub recipient: MailboxId,
    pub kind: MailKind,
    pub payload: Vec<u8>,
    pub count: u32,
    pub sender: SessionToken,
    pub from_component: Option<MailboxId>,
}

impl Mail {
    pub fn new(recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) -> Self {
        Self {
            recipient,
            kind,
            payload,
            count,
            sender: SessionToken::NIL,
            from_component: None,
        }
    }

    /// Attach a Claude session token as the sender. Used by the hub
    /// client when forwarding `HubToEngine::Mail`; other mail paths
    /// leave the default `NIL`.
    pub fn with_sender(mut self, sender: SessionToken) -> Self {
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
