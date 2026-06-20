//! [`OutboundReply`] — per-handler reply surface on top of [`MailSender`].
//!
//! Per-stage capability trait under the issue 663 refactor. Per-handler
//! ctxs (the FFI runtime `Ctx`, substrate's `NativeCtx`) impl this; init
//! and drop ctxs deliberately do not — there's no inbound mail at boot
//! and reply targets are not honoured during teardown.

use aether_data::{Kind, MailboxId};

use crate::actor::ctx::mail_sender::MailSender;

/// Per-handler reply surface, on top of [`MailSender`]. Handlers call
/// [`Self::reply::<K>(&payload)`][Self::reply] to answer the inbound's
/// originator without rethreading the per-call sender argument; the
/// ctx pulled the inbound's reply target out of the dispatcher and
/// stashed it internally before the handler ran.
///
/// Init contexts deliberately don't implement this — there's no
/// inbound-mail context at boot time. Drop contexts also do not —
/// reply handles invalidate on teardown.
pub trait OutboundReply: MailSender {
    /// Per-impl reply-handle type. The wasm-side ctx pins this to
    /// [`crate::mail::ReplyHandle`] (an opaque `u32` host-supplied
    /// handle); substrate's `NativeCtx` pins it to
    /// `aether_data::Source` (the structured `addr + correlation_id`
    /// that `Mailer::send_reply` consumes). The two shapes carry
    /// different information — issue 663 declines to unify them on a
    /// single concrete type and instead lets each impl surface its
    /// own.
    type ReplyHandle;

    /// Reply target for the mail currently being dispatched. `None` for
    /// component-origin and broadcast-origin mail; `Some(_)` when the
    /// inbound carries a routable originator (Claude session, peer
    /// component, remote engine mailbox).
    fn reply_target(&self) -> Option<Self::ReplyHandle>;

    /// Immediate-sender mailbox of the mail currently being dispatched,
    /// or `None` for mail with no local sender (broadcast,
    /// substrate-generated, hub-bubbled). This is the *immediate*
    /// sender (one hop, the addressing layer's `Source`), not the chain
    /// origin — the origin lives in the tracing layer (`root` /
    /// `parent_mail`, ADR-0080). Useful for caps that want to attribute
    /// work to the sending component without going through the reply
    /// path.
    fn source_mailbox(&self) -> Option<MailboxId>;

    /// Reply to the originator of the mail currently being dispatched.
    /// No-op when there's no reply target. Wire shape (cast or postcard)
    /// follows `Kind::encode_into_bytes` (ADR-0100), so a reply needs
    /// only `K: Kind` — a `Pod`-without-`Serialize` cast kind is
    /// repliable.
    fn reply<K: Kind>(&mut self, payload: &K);

    /// Reply to an explicit `sender` rather than the dispatcher-stamped
    /// reply target. Used by the parked-sender pattern: caps that stash
    /// a [`Self::ReplyHandle`] from one inbound and answer it later from
    /// a different handler (e.g. `aether-tcp`'s pending-unbinds, where
    /// the bind ack waits for the listener's monitor notice before
    /// firing).
    ///
    /// Same wire-shape contract as [`Self::reply`]. Native impls route
    /// through `NativeBinding::send_reply_for_handler`; FFI impls route
    /// through `crate::wasm::bridge::mail::reply_mail`.
    fn reply_to<K: Kind>(&mut self, sender: Self::ReplyHandle, payload: &K);
}
