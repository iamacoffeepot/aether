//! The encoder and decoder hooks a `Blob` or `ProtocolPath` field reaches
//! (ADR-0238 decision 3, ADR-0231 §3).
//!
//! [`WireEncode::encode_to`](super::WireEncode::encode_to) and
//! [`WireDecode::decode_from`](super::WireDecode::decode_from) pass one hook
//! through the derive and every container, so each `Blob` field calls
//! [`Encoder::blob`] once on the way out. On the way in a decode reaches the
//! engine only through the two [`Decoder`] operations:
//! [`Decoder::resolve_blob`] once per tag-1 `Blob` field, and
//! [`Decoder::prove_route_covers`] once per `ProtocolPath`. The plain hooks,
//! `Vec<u8>` and `&[u8]`, write tag 0 and refuse both operations, exactly as
//! an empty [`DecodeCtx`] does; the in-process envelope encoder is the only
//! encoder that overrides `blob`, and a decode that resolves goes through
//! the [`DecodeCtx`] it was handed.

use alloc::vec::Vec;

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

    /// Prove that the live route at `path` publishes every one of `rows`
    /// ([`DecodeCtx::prove_route_covers`]). The default refuses.
    ///
    /// # Errors
    ///
    /// [`Error::ProtocolPathUnchecked`] naming the path.
    fn prove_route_covers(&self, path: &ErasedActorPath, rows: &[(KindId, ReplyContract)]) -> Result<(), Error> {
        let _ = rows;
        Err(Error::ProtocolPathUnchecked { path: path.clone() })
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

/// A [`Decoder`] over a slice that forwards both engine operations to a
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
}
