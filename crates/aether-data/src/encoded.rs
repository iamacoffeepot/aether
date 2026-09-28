//! [`Encoded<K>`]: wire bytes that only an encode of a `K` can produce.

use alloc::vec::Vec;
use core::marker::PhantomData;

use crate::Kind;

/// The wire bytes of one `K`, typed by the kind that produced them.
///
/// [`Encoded::new`] is the only constructor and [`Kind::encode_into_bytes`]
/// the only producer, so a value of this type proves its bytes are a `K`. A
/// sender that encodes on one thread and sends on another carries this in
/// place of a `(KindId, Vec<u8>)` pair, and the send takes its kind from `K`
/// rather than from a runtime id nothing checks against the bytes.
///
/// `encode_into_bytes` writes each `Blob` field inline, which every decode
/// accepts, so the bytes need no engine store to be read back.
pub struct Encoded<K> {
    bytes: Vec<u8>,
    _kind: PhantomData<fn() -> K>,
}

impl<K: Kind> Encoded<K> {
    /// Encode `payload` in the wire shape its kind declares.
    #[must_use]
    pub fn new(payload: &K) -> Self {
        Self { bytes: payload.encode_into_bytes(), _kind: PhantomData }
    }

    /// The encoded bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}
