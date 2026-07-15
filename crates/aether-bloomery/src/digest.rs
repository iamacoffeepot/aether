//! Content addressing (ADR-0149 §The value vocabulary).
//!
//! A [`Digest`] is a sha256 over a value's *canonical aether-wire bytes*
//! ([`aether_data::wire`], ADR-0118). The workspace already owns one
//! canonical encoding; digests reuse it rather than inventing a parallel
//! canonicalization that would drift. sha256 — not `aether-data`'s FNV-1a
//! id hashing — because content addressing needs collision resistance,
//! consistent with the hub binary store (ADR-0115).

use aether_data::wire::to_vec;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// A sha256 digest over a value's canonical aether-wire bytes.
///
/// The primitive of the derivation DAG: every artifact is addressed by its
/// digest and names its parents by theirs. `Ord` (via the raw 32 bytes)
/// gives blooms a canonical member order at seal time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wrap 32 raw digest bytes — for reconstructing a digest from storage
    /// or naming a fixed one in a test.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32 digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// sha256 of an already-encoded byte string.
    #[must_use]
    pub fn of_wire_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }
}

/// The digest of a value: sha256 over its canonical aether-wire encoding.
///
/// Canonical encoding is infallible for every bloom value — it fails only
/// when a length exceeds the `u32` ceiling (ADR-0118), which no control-plane
/// value approaches — so an encode error degrades to hashing empty input
/// rather than panicking in the `no_std` core.
#[must_use]
pub fn digest_of<T: Serialize + ?Sized>(value: &T) -> Digest {
    let bytes = to_vec(value).unwrap_or_default();
    Digest::of_wire_bytes(&bytes)
}
