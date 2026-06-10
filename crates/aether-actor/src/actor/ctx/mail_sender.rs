//! [`MailSender`] — outbound-mail surface every actor ctx exposes.
//!
//! Per-stage capability trait (issue 663 + 665). Both init-time and
//! runtime ctxs across every transport implement [`MailSender`]; each
//! per-host concrete ctx struct (FFI: `FfiCtx` / `FfiInitCtx` /
//! `FfiDropCtx`; substrate: `NativeCtx` / `NativeInitCtx`) provides
//! its own bodies — there are no default-impl bodies because the
//! cross-target dispatch trait that backed them (`MailTransport`)
//! retired in 665. Each side calls its dispatch surface inline:
//! FFI bodies hit [`crate::ffi::bridge::MAIL_BRIDGE`], native bodies hit
//! `NativeBinding`'s inherent `send_mail`.
//!
//! `actor::<R>()` / `resolve_actor::<R>(name)` retired from this trait
//! because the returned typed-mailbox handle is per-side
//! ([`crate::ffi::FfiActorMailbox<R>`] vs `NativeActorMailbox<'a, R>`).
//! Each ctx provides them as inherent methods returning its own
//! per-side type; the everyday user-facing `ctx.actor::<R>().send(&payload)`
//! chain is unchanged. Generic-bounded code that needs cross-impl
//! sends uses the trait's [`MailSender::send`] / [`MailSender::send_many`]
//! / [`MailSender::send_to_named`] methods.

use aether_data::Kind;

use crate::actor::{HandlesKind, Singleton};

/// Outbound-mail surface every actor ctx exposes.
///
/// `R: Singleton + HandlesKind<K>` is the compile-time gate: trying to
/// send a kind the receiver doesn't handle is rejected at the call
/// site, not silently warn-dropped at runtime. The receiver's mailbox
/// is resolved through `R::resolve(caller_carry)` (ADR-0099 §5) — the
/// same lineage-aware path `ctx.actor::<R>()` walks — so a non-root
/// receiver routes to the lineage-folded id, not the flat
/// `hash(R::NAMESPACE)`. Wire shape (cast or postcard) follows
/// `Kind::encode_into_bytes` (issue #240).
pub trait MailSender {
    /// Send a single payload of kind `K` to the singleton instance of
    /// receiver actor `R`, resolved via `R::resolve` against the
    /// caller's lineage carry (ADR-0099 §5).
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind;

    /// Send a slice of cast-shape payloads as a contiguous batch.
    /// Cast-only — postcard has no efficient batched wire shape.
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit;

    /// String-keyed escape hatch for callers that genuinely don't
    /// know the receiver type at compile site (debug tools, dynamic
    /// dispatch, components addressing user-named mailboxes the
    /// substrate registered without a corresponding Rust type).
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K);

    /// Correlation id the host minted for this actor's most recent
    /// outbound `send_mail` (ADR-0042). `0` before any send.
    /// Universal mail-level metadata — every send mints a
    /// correlation, so the accessor lives on the outbound-mail trait.
    /// A handler tracks a request/reply round trip by stashing this id
    /// after the send and matching it against the inbound reply's
    /// correlation when the reply arrives in a later handler invocation.
    fn prev_correlation(&self) -> u64;

    /// ADR-0080 §7 fire-and-forget escape hatch: send `payload` to `R`
    /// without inheriting the caller's in-flight causal chain. The
    /// recipient processes the mail as the root of a new tree.
    ///
    /// **Fire-and-forget only.** Detached sends mint no parent linkage,
    /// so any reply the recipient issues inherits the *recipient's*
    /// tree rather than the sender's. Reply-correlated requests always
    /// go through [`Self::send`].
    ///
    /// Ships only the API surface (default body delegates to
    /// [`Self::send`]); a tracing-aware impl can specialise it to
    /// suppress the `TraceEvent::Sent` push that `send` would otherwise
    /// emit, for chassis-internal mail that must not appear in the causal
    /// trace.
    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        self.send::<R, K>(payload);
    }

    /// String-keyed counterpart to [`Self::send_detached`]. Same
    /// fire-and-forget contract; same default-body delegation in PR 1.
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        self.send_to_named::<K>(name, payload);
    }
}
