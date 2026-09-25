//! The encoder and decoder hooks a `Blob` field reaches (ADR-0238 decision 3).
//!
//! [`WireEncode::encode_to`](super::WireEncode::encode_to) and
//! [`WireDecode::decode_from`](super::WireDecode::decode_from) pass one hook
//! through the derive and every container, so each `Blob` field calls
//! [`Encoder::blob`] once on the way out and [`Decoder::resolve`] once per
//! tag-1 field on the way in. The plain hooks, `Vec<u8>` and `&[u8]`, write
//! tag 0 and refuse tag 1; the in-process envelope encoder is the only
//! encoder that overrides `blob`, and a decode resolves a tag-1 hash only
//! through the [`BlobResolver`] it was handed.

use alloc::vec::Vec;

use super::Error;
use crate::blob::{self, Blob, BlobHash};

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

/// Where a decode reads: the input cursor plus the tag-1 hook.
pub trait Decoder<'de> {
    /// The input cursor.
    fn cursor(&mut self) -> &mut &'de [u8];

    /// Resolve a tag-1 field's hash to its value. The default refuses.
    ///
    /// # Errors
    ///
    /// [`Error::DetachedBlob`] naming the hash.
    fn resolve(&mut self, hash: BlobHash) -> Result<Blob, Error> {
        Err(Error::DetachedBlob(hash))
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

/// A [`Decoder`] over a slice that resolves tag-1 hashes through a
/// [`BlobResolver`]. The resolver is a trait object, so each kind has one
/// decode body however many resolvers exist.
pub struct Resolving<'de, 'r> {
    cursor: &'de [u8],
    resolver: &'r mut dyn BlobResolver,
}

impl<'de, 'r> Resolving<'de, 'r> {
    /// A decoder over `cursor` that resolves through `resolver`.
    pub fn new(cursor: &'de [u8], resolver: &'r mut dyn BlobResolver) -> Self {
        Self { cursor, resolver }
    }

    /// Whether every input byte was consumed.
    pub fn is_empty(&self) -> bool {
        self.cursor.is_empty()
    }
}

impl<'de> Decoder<'de> for Resolving<'de, '_> {
    fn cursor(&mut self) -> &mut &'de [u8] {
        &mut self.cursor
    }

    fn resolve(&mut self, hash: BlobHash) -> Result<Blob, Error> {
        self.resolver.resolve(hash)
    }
}
