//! [`MailboxForward`] — the one send-shim a capability's sender facade sits on.
//!
//! A cap that offers a facade (`ctx.actor::<FsCapability>().read(ns, path)`)
//! has to reach four handle shapes: [`WasmActorMailbox`] /
//! [`WasmActorMailboxWithContext`] here, and the native pair in
//! `aether_substrate::actor::native`. Their `send` signatures differ only in
//! the return value — one is `()`, the other a `#[must_use]` correlation id —
//! so a facade written as an inherent method has to be written out four times.
//!
//! [`MailboxForward<A>`] normalizes that difference away: one fire-and-forget
//! `forward` per handle shape, defined once here (and once in the substrate for
//! the native pair, which owns those types). A facade then declares its methods
//! *once* as defaults on `trait FsMailboxExt: MailboxForward<FsCapability>` and
//! blankets them over every forwarder, so all four handle shapes pick the facade
//! up and the cap crate names no mailbox type at all.
//!
//! The `A: HandlesKind<K>` bound rides through unchanged, so the compile-time
//! wrong-kind rejection a facade method inherits is the same one a direct
//! `.send(&payload)` gets.

use aether_data::Kind;

use crate::model::{Addressable, HandlesKind};
use crate::wasm::{WasmActorMailbox, WasmActorMailboxWithContext};

/// Fire-and-forget send to actor `A`, uniform across every mailbox handle
/// shape that can address it.
///
/// Implemented for the wasm handles here and for the native pair in
/// `aether-substrate`; a capability's sender facade takes this as its
/// supertrait rather than impl'ing itself once per handle.
pub trait MailboxForward<A> {
    /// Send `payload` to `A`, discarding whatever the underlying handle's
    /// `send` returns. Gated on `A: HandlesKind<K>` exactly as the handle's own
    /// `send` is, so a facade method built on this rejects a kind the cap does
    /// not declare at the call site.
    fn forward<K>(&self, payload: &K)
    where
        A: HandlesKind<K>,
        K: Kind;
}

impl<A: Addressable> MailboxForward<A> for WasmActorMailbox<'_, A> {
    fn forward<K>(&self, payload: &K)
    where
        A: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

impl<A: Addressable, C: Kind> MailboxForward<A> for WasmActorMailboxWithContext<'_, '_, A, C> {
    fn forward<K>(&self, payload: &K)
    where
        A: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}
