//! Content-addressed artifacts above a raw blob store, and the typed
//! citations that name them.
//!
//! The store stays raw: a stored blob is a row naming `sha256(bytes)` plus a
//! file of exactly those bytes, named by that digest. An artifact is an
//! abstraction above that blob, whose bytes are an eight-byte [`KindId`]
//! prefix followed by a payload. The digest covers the kind, so a [`Digest`]
//! names one kind and one payload, and a [`Ref`] or [`ErasedRef`] cites it
//! through [`Cites`](super::Cites).
//!
//! [`KindId`]: crate::KindId

mod citation;
mod digest;
mod framing;
mod leaf_kinds;

pub use citation::{ErasedRef, Ref};
pub use digest::Digest;
pub use framing::{ArtifactHasher, artifact_blob, artifact_digest, artifact_prefix, hash_bytes};
pub use leaf_kinds::{OpaqueBytes, Utf8Text};

#[cfg(test)]
mod tests;
