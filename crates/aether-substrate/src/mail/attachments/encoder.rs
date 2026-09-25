//! The in-process envelope encoder (ADR-0238 decisions 3 and 4).
//!
//! An in-process send encodes its payload here. Each `Blob` field is interned
//! into the engine store, its entry is attached to the envelope, and the field
//! is written as tag 1 with the entry's hash, so the recipient shares the
//! bytes instead of copying them. It is called only where the destination is
//! known to be in-process; every path that leaves the process encodes with
//! the plain encoder, which writes tag 0.
//!
//! - A `Shared` value backed by this engine's store already holds its entry.
//!   The encoder reaches it through the hidden backing accessor and an `Any`
//!   downcast to [`BlobEntry`], with no lookup by hash and no copy.
//! - Any other value is checked into the store once: the one copy ADR-0238
//!   accepts for an `Owned` value's first in-process send. Check-in dedups by
//!   hash, so equal bytes share one entry.
//! - An entry is attached once however many fields name it.
//!
//! A payload with no `Blob` field never reaches the hook, so it allocates no
//! attachments.

use std::any::Any;
use std::sync::Arc;

use aether_data::wire::{Encoder, Error};
use aether_data::{Blob, BlobBacking, BlobReader, Kind};

use super::Attachments;
use crate::store::{BlobEntry, BlobStore};

/// The tag a `Blob` field carries when it names an attached entry by hash.
const TAG_HASH: u8 = 1;

/// The in-process encoder: interns each `Blob` field, attaches its entry,
/// writes tag 1. See the module docs.
struct EnvelopeEncoder<'s> {
    out: Vec<u8>,
    store: &'s BlobStore,
    attachments: Vec<Arc<BlobEntry>>,
}

impl Encoder for EnvelopeEncoder<'_> {
    fn out(&mut self) -> &mut Vec<u8> {
        &mut self.out
    }

    fn blob(&mut self, value: &Blob) -> Result<(), Error> {
        let entry = match store_entry(value) {
            Some(entry) => entry,
            None => self.store.check_in(read_all(value)?),
        };

        self.out.push(TAG_HASH);
        self.out.extend_from_slice(entry.hash().as_bytes());
        if !self.attachments.iter().any(|attached| attached.hash() == entry.hash()) {
            self.attachments.push(entry);
        }
        Ok(())
    }
}

/// An in-process payload and the entries its tag-1 fields name by hash.
pub struct EncodedMail {
    pub bytes: Vec<u8>,
    pub attachments: Attachments,
}

/// Encode `payload` for an in-process send against `store`. `attachments` is
/// `None` when no `Blob` field was encoded.
///
/// # Panics
///
/// When a length exceeds the `u32` ceiling, the one way a wire encode fails.
pub fn encode_envelope<K: Kind>(store: &BlobStore, payload: &K) -> EncodedMail {
    let mut encoder = EnvelopeEncoder { out: Vec::new(), store, attachments: Vec::new() };
    payload.encode_with(&mut encoder).expect("wire encode to Vec fails only past the u32 length ceiling");
    let EnvelopeEncoder { out, attachments, .. } = encoder;
    EncodedMail { bytes: out, attachments: (!attachments.is_empty()).then(|| attachments.into_boxed_slice()) }
}

/// The store entry behind a `Shared` value, recovered by downcast (ADR-0238
/// decision 4). `None` for `Owned` bytes, or a backing that is not a store
/// entry.
fn store_entry(value: &Blob) -> Option<Arc<BlobEntry>> {
    let backing: Arc<dyn BlobBacking> = Arc::clone(aether_data::__shared_backing(value)?);
    let backing: Arc<dyn Any + Send + Sync> = backing;
    backing.downcast::<BlobEntry>().ok()
}

/// Every byte of `value`, streamed into a buffer of its exact length.
fn read_all(value: &Blob) -> Result<Box<[u8]>, Error> {
    let reader = BlobReader::open(value);
    let len = usize::try_from(reader.len()).map_err(|_| Error::Length)?;
    let mut bytes = vec![0; len].into_boxed_slice();
    let mut filled = 0;
    while filled < len {
        let copied = reader.read_range(filled as u64, &mut bytes[filled..]);
        if copied == 0 {
            return Err(Error::UnexpectedEof);
        }
        filled += copied;
    }
    Ok(bytes)
}
