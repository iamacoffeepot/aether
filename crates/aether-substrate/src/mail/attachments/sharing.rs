//! Test support: encode a kind the way an attached in-process send writes
//! it, before the envelope encoder that will produce attachments in
//! production exists (ADR-0238 decision 3).
//!
//! A `Shared` value backed by this engine's store is written as tag 1 with
//! its hash, and its entry is attached once however many fields name it.
//! Every other value is written as tag 0, as the plain encoder writes it.
//! Tests use it to build the tag-1 payloads and attachments that the
//! plain-bytes consumers must rewrite.

use std::any::Any;
use std::sync::Arc;

use aether_data::wire::{Encoder, Error, WireEncode};
use aether_data::{Blob, BlobBacking, Kind};

use super::{Attachments, normalized};
use crate::store::BlobEntry;

/// An encoder that shares this engine's store entries by hash. See the module
/// docs.
pub struct SharingEncoder {
    out: Vec<u8>,
    attachments: Vec<Arc<BlobEntry>>,
}

impl SharingEncoder {
    /// `value`'s payload with each store-backed `Blob` field written as tag 1,
    /// plus the entries those fields name.
    ///
    /// # Panics
    ///
    /// When `value` does not encode: a test fixture that cannot encode is a
    /// broken test.
    pub fn encode<K: Kind>(value: &K) -> (Vec<u8>, Attachments) {
        let mut encoder = Self { out: Vec::new(), attachments: Vec::new() };
        value.encode_with(&mut encoder).expect("a test fixture encodes");
        (encoder.out, normalized(Some(encoder.attachments.into_boxed_slice())))
    }
}

impl Encoder for SharingEncoder {
    fn out(&mut self) -> &mut Vec<u8> {
        &mut self.out
    }

    fn blob(&mut self, value: &Blob) -> Result<(), Error> {
        let entry = aether_data::__shared_backing(value).and_then(|backing| {
            let backing: Arc<dyn BlobBacking> = Arc::clone(backing);
            let backing: Arc<dyn Any + Send + Sync> = backing;
            backing.downcast::<BlobEntry>().ok()
        });
        let Some(entry) = entry else {
            return value.encode_to(&mut self.out);
        };

        self.out.push(1);
        self.out.extend_from_slice(entry.hash().as_bytes());
        if !self.attachments.iter().any(|attached| attached.hash() == entry.hash()) {
            self.attachments.push(entry);
        }
        Ok(())
    }
}
