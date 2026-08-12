//! Backend-object correspondence owned by the Bloomery domain.
//!
//! A Bloomery [`Digest`] identifies a canonical Bloomery value, while a source
//! or executor backend addresses the object carrying that value in its own
//! namespace. The domain deliberately keeps that backend identifier opaque:
//! adapters validate and interpret its bytes at their boundary.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use std::error::Error;

use crate::Digest;

/// An opaque object identifier owned by a backend.
///
/// Bloomery stores and compares these bytes without assigning them a format.
/// A concrete adapter is responsible for validating the byte shape before it
/// passes the identifier to an external API or renders it for a human-facing
/// surface.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct BackendObjectId(Vec<u8>);

impl BackendObjectId {
    /// Own an adapter-provided object identifier without interpreting it.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrow the opaque backend bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Recover ownership of the opaque backend bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// A correspondence-store fault.
///
/// A clean absent correspondence is returned as `Ok(None)`; this type is for a
/// durable read/write failure or for bytes that the consuming adapter cannot
/// decode as one of its object identifiers.
#[derive(Debug)]
pub struct CorrespondenceError {
    message: String,
}

impl CorrespondenceError {
    /// Wrap a storage or decode fault description.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for CorrespondenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "backend correspondence store: {}", self.message)
    }
}

impl Error for CorrespondenceError {}

/// The persisted two-way mapping between Bloomery values and backend objects.
///
/// Implementations use interior mutability because source and executor ports
/// share the correspondence and drive it through `&self` methods.
pub trait Correspondence {
    /// Record that `backend_object` carries `digest`.
    ///
    /// Both axes are last-writer-wins: re-pointing either value retires the old
    /// reverse mapping, and recording the same pair again is idempotent.
    ///
    /// # Errors
    /// The durable store could not be written.
    fn record(&self, digest: &Digest, backend_object: &BackendObjectId) -> Result<(), CorrespondenceError>;

    /// Resolve the backend object carrying `digest`, if one was recorded.
    ///
    /// # Errors
    /// The durable store could not be read.
    fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError>;

    /// Resolve the Bloomery digest carried by `backend_object`, if recorded.
    ///
    /// # Errors
    /// The durable store could not be read.
    fn resolve_digest(&self, backend_object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError>;
}

/// A correspondence shared by source and executor ports.
pub type SharedCorrespondence = Arc<dyn Correspondence + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::{BackendObjectId, CorrespondenceError};

    #[test]
    fn backend_object_id_owns_exposes_and_returns_opaque_bytes() {
        let source = vec![1, 2, 3, 4];
        let object = BackendObjectId::new(source.clone());

        assert_eq!(object.as_bytes(), source);
        assert_eq!(object.into_bytes(), source);
    }

    #[test]
    fn correspondence_error_is_domain_neutral() {
        assert_eq!(
            CorrespondenceError::new("disk unavailable").to_string(),
            "backend correspondence store: disk unavailable",
        );
    }
}
