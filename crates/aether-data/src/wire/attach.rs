//! The encoder and decoder hooks a `Blob`, `ProtocolPath` or held-reply
//! field reaches (ADR-0238 decision 3, ADR-0231 §3, ADR-0243).
//!
//! [`WireEncode::encode_to`](super::WireEncode::encode_to) and
//! [`WireDecode::decode_from`](super::WireDecode::decode_from) pass one hook
//! through the derive and every container, so each `Blob` field calls
//! [`Encoder::blob`] once and each held ticket calls [`Encoder::held`] once on
//! the way out. On the way in a decode reaches the engine only through the
//! three [`Decoder`] operations: [`Decoder::resolve_blob`] once per tag-1
//! `Blob` field, [`Decoder::prove_route_covers`] once per `ProtocolPath`, and
//! [`Decoder::claim_held`] once per held ticket. The plain hooks, `Vec<u8>`
//! and `&[u8]`, write tag 0, refuse a held ticket and refuse every decode
//! operation, exactly as an empty [`DecodeCtx`] does. The in-process envelope
//! encoder is the only encoder that overrides `blob`, [`LedgerEncoder`] is the
//! only one that overrides `held`, and a decode that resolves or claims goes
//! through the [`DecodeCtx`] it was handed.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;

use super::{DecodeCtx, Error};
use crate::blob::{self, Blob, BlobHash};
use crate::{ErasedActorPath, KindId, ReplyContract};

/// Where an encode writes: the output buffer plus the `Blob` field hook.
pub trait Encoder {
    /// The output buffer.
    fn out(&mut self) -> &mut Vec<u8>;

    /// Write one `Blob` field. The default writes tag 0 and the bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Length`] when the blob's length exceeds the `u32` ceiling.
    fn blob(&mut self, value: &Blob) -> Result<(), Error> {
        blob::encode_inline(self.out(), value)
    }

    /// Write one held ticket, an obligation to answer one `reply` (ADR-0243).
    /// The default refuses and writes nothing, so a held ticket encoded
    /// anywhere but a [`LedgerEncoder`] fails instead of leaving the ledger.
    ///
    /// # Errors
    ///
    /// [`Error::HeldUngranted`] naming `reply`, or the granting ledger's
    /// refusal.
    fn held(&mut self, ticket: u64, reply: KindId) -> Result<(), Error> {
        let _ = ticket;
        Err(Error::HeldUngranted { reply })
    }
}

impl Encoder for Vec<u8> {
    fn out(&mut self) -> &mut Vec<u8> {
        self
    }
}

/// Where a decode reads: the input cursor plus the two engine operations.
pub trait Decoder<'de> {
    /// The input cursor.
    fn cursor(&mut self) -> &mut &'de [u8];

    /// Resolve a tag-1 `Blob` field's hash to its value. The default refuses.
    ///
    /// # Errors
    ///
    /// [`Error::DetachedBlob`] naming the hash.
    fn resolve_blob(&mut self, hash: BlobHash) -> Result<Blob, Error> {
        Err(Error::DetachedBlob(hash))
    }

    /// Prove that the route at `path` publishes every one of `rows`
    /// ([`DecodeCtx::prove_route_covers`]). The default refuses.
    ///
    /// # Errors
    ///
    /// [`Error::ProtocolPathUnchecked`] naming the path.
    fn prove_route_covers(&self, path: &ErasedActorPath, rows: &[(KindId, ReplyContract)]) -> Result<(), Error> {
        let _ = rows;
        Err(Error::ProtocolPathUnchecked { path: path.clone() })
    }

    /// Claim the held ticket a decoded field carries back from the granting
    /// ledger ([`DecodeCtx::claim_held`]). The default refuses.
    ///
    /// # Errors
    ///
    /// [`Error::HeldUngranted`] naming `reply`.
    fn claim_held(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error> {
        let _ = ticket;
        Err(Error::HeldUngranted { reply })
    }
}

impl<'de> Decoder<'de> for &'de [u8] {
    fn cursor(&mut self) -> &mut &'de [u8] {
        self
    }
}

/// What a decode resolves tag-1 hashes through: the attached entries on a
/// native delivery (#6748), a hold on the guest (#6749).
pub trait BlobResolver {
    /// The value whose hash is `hash`.
    ///
    /// # Errors
    ///
    /// [`Error::DetachedBlob`] when the resolver does not supply `hash`.
    fn resolve(&mut self, hash: BlobHash) -> Result<Blob, Error>;
}

/// What a granted ledger hands back for a claimed ticket: a value only the
/// granting runtime's `Held` decode knows how to read, so the data layer never
/// names the runtime's binding type.
pub struct HeldClaim(pub Box<dyn Any + Send>);

impl HeldClaim {
    /// The claimed value as `T`, or this claim unchanged when it holds
    /// another type.
    ///
    /// # Errors
    ///
    /// This claim, unchanged, when it does not hold a `T`.
    pub fn downcast<T: Any>(self) -> Result<T, Self> {
        self.0.downcast::<T>().map(|value| *value).map_err(Self)
    }
}

impl fmt::Debug for HeldClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HeldClaim(..)")
    }
}

/// The runtime ledger of outstanding held tickets (ADR-0243): parked by a
/// [`LedgerEncoder`], claimed back by a [`DecodeCtx::held`] decode.
pub trait HeldLedger {
    /// Record `ticket` as parked in encoded bytes while it waits to answer
    /// `reply`.
    ///
    /// # Errors
    ///
    /// [`Error::HeldUnclaimed`] when the ledger does not own `ticket` as an
    /// obligation to answer `reply`.
    fn park(&mut self, ticket: u64, reply: KindId) -> Result<(), Error>;

    /// Take `ticket` back out of the parked set for a decode.
    ///
    /// # Errors
    ///
    /// [`Error::HeldUnclaimed`] when `ticket` is not parked as an obligation
    /// to answer `reply`.
    fn claim(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error>;
}

/// The one [`Encoder`] that grants held tickets: [`Encoder::held`] parks the
/// ticket in its ledger and then writes it as a `u64` little-endian.
/// `blob` keeps the tag-0 default.
pub struct LedgerEncoder<'l> {
    out: Vec<u8>,
    ledger: &'l mut dyn HeldLedger,
}

impl<'l> LedgerEncoder<'l> {
    /// An empty encoder that parks held tickets in `ledger`.
    pub fn new(ledger: &'l mut dyn HeldLedger) -> Self {
        Self { out: Vec::new(), ledger }
    }

    /// The bytes written so far.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

impl Encoder for LedgerEncoder<'_> {
    fn out(&mut self) -> &mut Vec<u8> {
        &mut self.out
    }

    fn held(&mut self, ticket: u64, reply: KindId) -> Result<(), Error> {
        self.ledger.park(ticket, reply)?;
        self.out.extend_from_slice(&ticket.to_le_bytes());
        Ok(())
    }
}

/// A [`Decoder`] over a slice that forwards every engine operation to a
/// [`DecodeCtx`]. The context is borrowed whole, so each kind has one decode
/// body whatever the context carries.
pub struct InCtx<'de, 'x, 'c> {
    cursor: &'de [u8],
    ctx: &'x mut DecodeCtx<'c>,
}

impl<'de, 'x, 'c> InCtx<'de, 'x, 'c> {
    /// A decoder over `cursor` that resolves through `ctx`.
    pub fn new(cursor: &'de [u8], ctx: &'x mut DecodeCtx<'c>) -> Self {
        Self { cursor, ctx }
    }

    /// Whether every input byte was consumed.
    pub fn is_empty(&self) -> bool {
        self.cursor.is_empty()
    }
}

impl<'de> Decoder<'de> for InCtx<'de, '_, '_> {
    fn cursor(&mut self) -> &mut &'de [u8] {
        &mut self.cursor
    }

    fn resolve_blob(&mut self, hash: BlobHash) -> Result<Blob, Error> {
        self.ctx.resolve_blob(hash)
    }

    fn prove_route_covers(&self, path: &ErasedActorPath, rows: &[(KindId, ReplyContract)]) -> Result<(), Error> {
        self.ctx.prove_route_covers(path, rows)
    }

    fn claim_held(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error> {
        self.ctx.claim_held(ticket, reply)
    }
}
