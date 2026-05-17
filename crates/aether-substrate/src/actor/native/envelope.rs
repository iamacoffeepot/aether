//! The owned-bytes envelope shape native dispatchers receive on their
//! mpsc inbox. Sinks today take borrowed args (`&str`, `&[u8]`); routing
//! across an mpsc channel forces ownership.

use crate::mail::registry::OwnedDispatch;
use crate::mail::{KindId, MailId, ReplyTo};

/// One mail delivered to a capability through its mpsc receiver.
///
/// Sinks today receive borrowed args (`&str`, `&[u8]`); routing across
/// an mpsc channel forces ownership. Capabilities that care about
/// ergonomics destructure this once at the top of their loop.
///
/// `mail_id`, `root`, and `parent_mail` (ADR-0080 §1 / §5) carry the
/// inbound mail's identity and causal-chain pointers. The native
/// dispatcher reads them to populate the per-handler `NativeCtx`'s
/// `in_flight()` accessors so child sends inherit the correct root.
/// PR 2 of issue #707 plumbs the data; PR 2's TraceObserver hooks
/// emit `TraceEvent::Received { mail_id, .. }` against the same value
/// the producer's `Sent` event carried.
#[derive(Debug)]
pub struct Envelope {
    pub kind: KindId,
    pub kind_name: String,
    pub origin: Option<String>,
    pub sender: ReplyTo,
    pub payload: Vec<u8>,
    pub count: u32,
    pub mail_id: MailId,
    pub root: MailId,
    pub parent_mail: Option<MailId>,
}

/// Issue iamacoffeepot/aether#848 PR 3: move every field out of
/// the inbound [`OwnedDispatch`] into a fresh [`Envelope`]. No
/// allocations — payload + kind_name + origin all transfer
/// ownership. Replaces the legacy `build_envelope(&MailDispatch)`
/// path inside production cap registration closures, which paid
/// `Vec<u8>` + `String` clones per dispatch.
impl From<OwnedDispatch> for Envelope {
    #[inline]
    fn from(dispatch: OwnedDispatch) -> Self {
        Self {
            kind: dispatch.kind,
            kind_name: dispatch.kind_name,
            origin: dispatch.origin,
            sender: dispatch.sender,
            payload: dispatch.payload,
            count: dispatch.count,
            mail_id: dispatch.mail_id,
            root: dispatch.root,
            parent_mail: dispatch.parent_mail,
        }
    }
}
