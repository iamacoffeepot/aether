//! [`OutboundReply`] — per-handler reply surface on top of [`MailSender`].
//!
//! Per-stage capability trait under the issue 663 refactor. Per-handler
//! ctxs (the FFI runtime `Ctx`, substrate's `NativeCtx`) impl this; init
//! and drop ctxs deliberately do not — there's no inbound mail at boot
//! and reply targets are not honoured during teardown.
//!
//! Phase A of issue 663 adds the trait alongside the existing
//! [`crate::actor::sender::MailCtx`] which still backs current call
//! sites; Phase B impls it on the existing ctx types so the trait is
//! reachable everywhere `MailCtx` is reachable; Phase D drops the
//! pre-issue-663 `MailCtx` once the generic-bounds API has converged.

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
    /// [`crate::mail::ReplyTo`] (an opaque `u32` host-supplied
    /// handle); substrate's `NativeCtx` pins it to
    /// `aether_data::ReplyTo` (the structured `target + correlation_id`
    /// that `Mailer::send_reply` consumes). The two shapes carry
    /// different information — issue 663 declines to unify them on a
    /// single concrete type and instead lets each impl surface its
    /// own.
    type ReplyHandle;

    /// Reply target for the mail currently being dispatched. `None` for
    /// component-origin and broadcast-origin mail; `Some(_)` when the
    /// inbound carries a routable originator (Claude session, peer
    /// component, remote engine mailbox).
    fn reply_to(&self) -> Option<Self::ReplyHandle>;

    /// Local-component origin of the mail currently being dispatched,
    /// or `None` for mail with no local sender (broadcast,
    /// substrate-generated, hub-bubbled). Useful for caps that want
    /// to attribute work to the originating component without going
    /// through the reply path.
    fn origin(&self) -> Option<MailboxId>;

    /// Reply to the originator of the mail currently being dispatched.
    /// No-op when there's no reply target. Wire shape (cast or postcard)
    /// follows `Kind::encode_into_bytes`.
    ///
    /// The `serde::Serialize` bound matches the substrate-side
    /// `Mailer::send_reply` requirement: reply kinds route through
    /// postcard when the target is a Claude session or remote engine
    /// mailbox. Every reply kind in the workspace already derives
    /// `Serialize` so the bound is documentation, not a breaking
    /// change.
    fn reply<K: Kind + serde::Serialize>(&mut self, payload: &K);
}
