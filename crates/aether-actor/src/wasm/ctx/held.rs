//! A wasm guest's typed deferred reply (ADR-0243 §6): the [`Pending`]
//! receipt a `#[handler::single]` returns, and the move-only [`Held`] ticket
//! that answers it later.
//!
//! [`WasmCtx::hold`] mints the pair. The receipt goes back to the `#[actor]`
//! macro, which defuses it and reports `DISPATCH_HANDLED_HOLD`, so the host
//! keeps the dispatch's reply handle and holds the requester's settlement.
//! The ticket is that reply handle. It lives in the actor's state, or travels
//! by value in a request context or in saved state, and [`Held::answer`]
//! sends the one reply it owes.
//!
//! A ticket encodes only through the codec hooks the runtime grants
//! (`Encoder::held` / `DecodeCtx::claim_held`): the request-context table and
//! the dehydrate encoder park it, and a context take or a rehydrate decode
//! claims it back. Any other encode refuses and leaves the ticket armed.
//! Dropping a ticket that is still armed, or a receipt the macro did not
//! defuse, panics (ADR-0063).

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::mem;

use aether_data::wire::{self, Decoder, Encoder, WireDecode, WireEncode};
use aether_data::{ActorMail, CastEligible, Kind, LabelNode, Schema, SchemaType};
use alloc::vec::Vec;

use super::WasmCtx;
use crate::blob::guest::encode_guest;
use crate::mail::{NO_REPLY_HANDLE, ReplyHandle};
use crate::model::ctx::reply_mode::{ReplyMode, Single};
use crate::wasm::bridge::mail;

/// The receipt a single handler returns for a reply it answers later
/// (ADR-0243 §2): `-> Pending<R>` declares that the handler's row replies
/// `R`, through the [`Held<R>`] minted beside it.
///
/// It is phantom and holds nothing. The `#[actor]` macro defuses the one its
/// handler returns. A receipt dropped anywhere else panics, because a handler
/// that armed a hold and discarded the receipt would declare a reply it
/// never reports.
#[must_use = "return the receipt from the handler: the `#[actor]` macro defuses it"]
pub struct Pending<R> {
    _reply: PhantomData<fn() -> R>,
}

impl<R> Pending<R> {
    pub(super) const fn new() -> Self {
        Self { _reply: PhantomData }
    }

    /// Not part of the public API; the `#[actor]` macro calls it on the
    /// receipt a `-> Pending<R>` handler returned.
    #[doc(hidden)]
    pub fn __defuse(self) {
        mem::forget(self);
    }
}

impl<R> Drop for Pending<R> {
    fn drop(&mut self) {
        panic!(
            "aether-actor: a `Pending` receipt was dropped instead of returned from its handler; return it so the \
             `#[actor]` macro reports the held reply"
        );
    }
}

impl<R> fmt::Debug for Pending<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pending(..)")
    }
}

/// A move-only obligation to answer one `R` (ADR-0243 §4, §6). Its ticket is
/// the reply handle of the dispatch that minted it; the host keeps that
/// handle, and the requester's settlement, until [`Self::answer`] sends the
/// reply.
///
/// A `Held` is a kind field of actor reach: it implements neither
/// `CrossesActors` nor `CrossesWire`, so a context or state kind that holds
/// one is never mail. Its schema is `SchemaType::Ticket { reply: R::ID }`.
///
/// Dropping a `Held` that is still armed panics. A granted encoder (a stored
/// request context, or saved state) sets its parked flag, and a parked
/// `Held` drops silently: the encoded bytes now carry the debt. A `Held`
/// minted for mail with no reply target is detached; it answers nothing and
/// drops silently.
pub struct Held<R> {
    ticket: u32,
    parked: Cell<bool>,
    _reply: PhantomData<fn() -> R>,
}

impl<R> Held<R> {
    pub(super) const fn new(ticket: u32) -> Self {
        Self { ticket, parked: Cell::new(false), _reply: PhantomData }
    }

    /// Whether a granted encoder has parked this ticket in encoded bytes.
    #[cfg(test)]
    pub(super) fn is_parked(&self) -> bool {
        self.parked.get()
    }

    /// The reply handle this ticket answers.
    #[cfg(test)]
    pub(super) const fn ticket(&self) -> u32 {
        self.ticket
    }

    /// Whether this ticket came from mail without a reply target.
    ///
    /// Not part of the public API; generated bundle roots use this to retain
    /// the historical behavior of ignoring one-way requests before starting
    /// work that might need a deferred reply.
    #[doc(hidden)]
    #[must_use]
    pub const fn __is_detached(&self) -> bool {
        self.ticket == NO_REPLY_HANDLE
    }
}

impl<R: ActorMail> Held<R> {
    /// Send the one reply this ticket owes and release its reply handle.
    /// A detached ticket sends nothing.
    ///
    /// `ctx` is the actor's own ctx, in any handler: a held reply answers on
    /// the actor that holds it (ADR-0243 §5).
    pub fn answer<A, M: ReplyMode>(self, ctx: &mut WasmCtx<'_, A, M>, reply: &R) {
        if self.ticket != NO_REPLY_HANDLE {
            let encoded = encode_guest(reply);
            mail::reply_mail(self.ticket, R::ID.0, &encoded.bytes, 1, ctx.mailbox);
            ctx.inline.release_held(self.ticket);
        }
        mem::forget(self);
    }
}

impl<R> Drop for Held<R> {
    fn drop(&mut self) {
        assert!(
            self.parked.get() || self.ticket == NO_REPLY_HANDLE,
            "aether-actor: a `Held` reply (handle {}) was dropped unanswered; answer it or park it in a request \
             context or saved state",
            self.ticket
        );
    }
}

impl<R> fmt::Debug for Held<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Held").field("ticket", &self.ticket).field("parked", &self.parked.get()).finish()
    }
}

impl<R: ActorMail> Schema for Held<R> {
    const SCHEMA: SchemaType = SchemaType::Ticket { reply: R::ID };
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<R: ActorMail> CastEligible for Held<R> {
    const ELIGIBLE: bool = false;
}

impl<R: ActorMail> WireEncode for Held<R> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), wire::Error> {
        self.encode_to(out)
    }

    /// Hand the ticket to `enc`. Only a granted encoder accepts it, and its
    /// acceptance parks this value, so dropping it afterwards is silent.
    fn encode_to<E: Encoder + ?Sized>(&self, enc: &mut E) -> Result<(), wire::Error> {
        enc.held(u64::from(self.ticket), R::ID)?;
        self.parked.set(true);
        Ok(())
    }
}

impl<'de, R: ActorMail> WireDecode<'de> for Held<R> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, wire::Error> {
        Self::decode_from(cursor)
    }

    /// Read the ticket and claim it back from the granted ledger, which
    /// makes it live again in this instance.
    fn decode_from<D: Decoder<'de> + ?Sized>(dec: &mut D) -> Result<Self, wire::Error> {
        let raw = u64::decode(dec.cursor())?;
        let ticket = u32::try_from(raw).map_err(|_| wire::Error::HeldUnclaimed { ticket: raw, reply: R::ID })?;
        dec.claim_held(raw, R::ID)?;
        Ok(Self::new(ticket))
    }
}

impl<A> WasmCtx<'_, A, Single> {
    /// Arm a deferred reply for the mail being dispatched (ADR-0243 §1, §6):
    /// return the [`Pending<R>`] receipt from the handler and keep the
    /// [`Held<R>`] to answer later, from this or any later handler.
    ///
    /// For mail with no reply target the ticket is detached: it answers
    /// nothing and drops silently.
    ///
    /// # Panics
    ///
    /// On a second `hold` in one dispatch, which would owe two replies to
    /// one request.
    pub fn hold<R: ActorMail>(&mut self) -> (Pending<R>, Held<R>) {
        assert!(!self.held_armed, "aether-actor: `hold` called twice in one dispatch; a request owes one reply");
        self.held_armed = true;

        let ticket = self.sender.map_or(NO_REPLY_HANDLE, ReplyHandle::raw);
        self.inline.arm_held(ticket, <R as Kind>::ID);
        (Pending::new(), Held::new(ticket))
    }
}
