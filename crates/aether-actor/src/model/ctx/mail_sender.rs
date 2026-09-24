//! [`MailSender`] — outbound-mail surface every actor ctx exposes.
//!
//! Per-stage capability trait (issue 663 + 665). Both init-time and
//! runtime ctxs across every transport implement [`MailSender`]; each
//! per-host concrete ctx struct (FFI: `WasmCtx` / `WasmInitCtx` /
//! `WasmDropCtx`; substrate: `NativeCtx` / `NativeInitCtx`) provides
//! its own bodies — there are no default-impl bodies because the
//! cross-target dispatch trait that backed them (`MailTransport`)
//! retired in 665. Each side calls its dispatch surface inline:
//! FFI bodies call `crate::wasm::bridge::mail::send_mail`, native bodies hit
//! `NativeBinding`'s inherent `send_mail`.
//!
//! The typed sends are not on this trait. Each ctx typed by its actor carries
//! them as inherent flat verbs (`ctx.send::<R>` and its siblings), bounded on
//! the actor's declared dependency (ADR-0232), and sends through a held proof
//! with its inherent `send_to`. What stays here is the by-proof detached send
//! and the correlation accessor, which the stored HTTP stream handles reach
//! through the trait.

use aether_data::ActorMail;

use crate::reference::ErasedActorRef;

/// Outbound-mail surface every actor ctx exposes: the correlation accessor
/// and the by-proof detached send.
///
/// The typed sends live on each ctx as inherent flat verbs rather than here.
/// A flat verb compiles only on a ctx typed by an actor that declares the
/// receiver `R` with `#[actor(depends(R))]`, and only for a kind `R` handles,
/// so an undeclared or wrong-kind send is rejected at the call site rather
/// than warn-dropped at runtime (ADR-0232). Wire shape (cast or structured)
/// follows `Kind::encode_into_bytes` (issue #240).
pub trait MailSender {
    /// Correlation id the host minted for this actor's most recent
    /// outbound `send_mail` (ADR-0042). `0` before any send.
    /// Universal mail-level metadata — every send mints a
    /// correlation, so the accessor lives on the outbound-mail trait.
    /// A handler tracks a request/reply round trip by stashing this id
    /// after the send and matching it against the inbound reply's
    /// correlation when the reply arrives in a later handler invocation.
    fn prev_correlation(&self) -> u64;

    /// Fire-and-forget send of `payload` to the proven `target`, minting a
    /// fresh causal root rather than inheriting the caller's in-flight chain
    /// (ADR-0080 §7).
    ///
    /// The send grid crosses typed / by-proof with inherit / detached. The
    /// typed cells are each ctx's inherent flat verbs (`send::<R>` and
    /// `send_detached::<R>`, bounded on a declared dependency); the by-proof
    /// cells are the inherent `send_to` and this method, its detached
    /// partner. There is no by-name column, because text is not a proof
    /// (ADR-0230). The by-proof cell takes the dispatch-stamped proof rather
    /// than a position anyone can compute, so a hand-built id does not reach
    /// it. Its motivating consumer is the stored stream handle that answers
    /// whoever dispatched to a handler (ADR-0133): the counterparty is
    /// `ctx.sender()`, captured at runtime, so a typed `R` can't name it and a
    /// fresh root is wanted per send. The kind is unchecked because that
    /// counterparty is deliberately untyped — a mock or a middleware stands in
    /// for the cap.
    ///
    /// **Fire-and-forget only.** A detached send mints no parent linkage, so
    /// any reply the recipient issues inherits the *recipient's* tree rather
    /// than the sender's. Reply-correlated requests do not use it.
    ///
    /// Required rather than defaulted: there is no by-id inherit method on
    /// this trait to delegate to (the inherit-by-id send is the per-ctx
    /// inherent `send_to`), so each concrete ctx supplies its own body.
    fn send_detached_to<K: ActorMail>(&mut self, target: ErasedActorRef, payload: &K);
}
