//! Typed stored-event trigger decoded from a journal [`Entry`].

use aether_bloomery_kinds::{DecodeError, Entry};
use aether_data::Storage;

/// Event that can start preparation. Decoded with the portable storage codec.
pub trait Trigger: Storage + Sized + 'static {
    /// Decode `entry` as `Self`.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the stored kind or payload does not match `Self`.
    fn from_entry(entry: &Entry) -> Result<Self, DecodeError> {
        entry.decode()
    }
}

impl<K: Storage + 'static> Trigger for K {}
