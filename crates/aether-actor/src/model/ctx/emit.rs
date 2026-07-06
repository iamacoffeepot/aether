//! [`Emit`] — the multi-class emission surface (ADR-0134).
//!
//! Per-stage capability trait, the multi-class counterpart of
//! [`OutboundReply`](crate::OutboundReply). A `#[handler::multi]` handler
//! receives a ctx in [`Multi<K>`](crate::Multi) mode, for which [`Emit<K>`]
//! is implemented (only for that mode); the handler answers one dispatch
//! with 0..n mails of the declared kind `K` by calling `emit`.
//!
//! Each emission is a **detached chain root addressed at the dispatch
//! source** — the sugar for `send_detached_to(source_mailbox, payload)`.
//! Emissions never ride the request chain: settlement stays prompt (the
//! request chain settles on the handler's return, not on the emissions),
//! and detached-always gives every emission the same chain shape
//! regardless of producer timing (ADR-0134, leaning on ADR-0133/0132's
//! detached data phase + payload correlation). A dispatch with no routable
//! source (session / broadcast / substrate-generated mail) warn-drops the
//! emission rather than fabricating a target.

use aether_data::Kind;

/// Multi-class emission surface (ADR-0134). Implemented only for a ctx in
/// [`Multi<K>`](crate::Multi) mode, so a handler that declares
/// `#[handler::multi]` with a `Multi<K>` ctx can `emit` 0..n `K` mails and
/// a handler of any other class cannot (a stray `emit` is a compile error,
/// not a manifest lie).
pub trait Emit<K: Kind> {
    /// Emit one `payload` of kind `K` as a detached chain root addressed at
    /// the mail's dispatch source (ADR-0134). Fire-and-forget: the emission
    /// starts a fresh causal chain rather than inheriting the handler's, so
    /// it does not hold the request chain open. A dispatch with no routable
    /// source (session / broadcast / substrate-generated mail) drops the
    /// emission with a warning.
    fn emit(&mut self, payload: &K);
}
