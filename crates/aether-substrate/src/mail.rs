// Mail envelope types. Owned by value because mails cross thread
// boundaries through the scheduler's queue.

/// Addressing token for any mailbox — component or substrate-owned sink.
/// Opaque `u32` newtype so it can't be accidentally mixed with wasmtime
/// indices or raw integers.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MailboxId(pub u32);

/// Host/guest contract tag for the payload layout. The substrate and the
/// components that talk to it agree on a specific layout per kind. Typed
/// facade over this is deferred to a later milestone per issue #18.
pub type MailKind = u32;

/// The transport envelope. `payload` is the exact byte layout the kind
/// implies; `count` is the number of items the layout implies, where
/// applicable.
#[derive(Debug)]
pub struct Mail {
    pub recipient: MailboxId,
    pub kind: MailKind,
    pub payload: Vec<u8>,
    pub count: u32,
}

impl Mail {
    pub fn new(recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) -> Self {
        Self {
            recipient,
            kind,
            payload,
            count,
        }
    }
}
