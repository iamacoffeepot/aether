//! TLV storage shape (ADR-0059): content-hashed field tags, flattening,
//! an unknown-fields bucket, and a nominal `Kind::ID`.
//!
//! Mail kinds keep the positional cast / structured codecs. A type
//! derives [`Storage`] *instead of* [`crate::Kind`], never alongside it.

mod hash;
mod leaf;
mod leaves;
mod record;

pub use hash::{
    BYTES_SCHEMA, MAX_STORAGE_DEPTH, U64_SCHEMA, UNIT_SCHEMA, VARIANT_LEAF, assert_unique_storage_leaves, count_leaves,
    field_hash, field_path_root, fold_dotted_path, fold_index_segment, fold_path_segment, nth_leaf_hash,
    terminate_field_hash, variant_hash,
};
pub use leaf::LeafBody;
pub use leaves::{
    StorageLeaves, assemble_bytes, assemble_bytes_with_aliases, assemble_with_aliases, bytes_absent, contribute_bytes,
};
pub use record::{RecordReader, RecordWriter, StorageError, UnknownField};

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::Schema;

use hash::field_hash as hash_field;
use leaf::{LeafBody as Body, decode_leaf};
use leaves::StorageLeaves as Leaves;
use record::{RecordReader as Reader, RecordWriter as Writer, StorageError as Error, UnknownField as Unknown};

/// Third wire shape: TLV records with content-hashed field tags.
///
/// `decode_from_bytes` keeps its yield-nothing default, so a storage
/// kind arriving as mail is an ordinary strict-receiver miss. The
/// derive emits an `encode_into_bytes` override that panics naming the
/// storage kind and the handle-indirection rule.
pub trait Storage: crate::Kind {
    /// Reject unknown fields rather than bucketing them. Default
    /// `false` (forgiving for storage forward-compat); `true` for
    /// payloads where silently carrying an unknown field is a
    /// security concern. Set via `#[storage(strict)]`.
    const STRICT: bool = false;

    /// Decode TLV bytes into a typed value plus any unknown fields.
    fn decode_storage(bytes: &[u8]) -> Result<StorageData<Self>, Error>
    where
        Self: Sized;

    /// Encode a typed value plus its unknown-field bucket.
    fn encode_storage(data: &StorageData<Self>) -> Result<Vec<u8>, Error>
    where
        Self: Sized;
}

/// Typed storage value beside the unknown-field bucket.
#[derive(Debug)]
pub struct StorageData<T> {
    /// Value assembled from the fields this schema binds.
    pub value: T,
    /// Records this schema does not bind, preserved for round-trip.
    pub unknown_fields: Vec<Unknown>,
    records: BTreeMap<u64, Vec<u8>>,
}

impl<T> StorageData<T> {
    /// Wrap a freshly assembled value.
    #[must_use]
    pub fn from_parts(value: T, unknown_fields: Vec<Unknown>, records: BTreeMap<u64, Vec<u8>>) -> Self {
        Self { value, unknown_fields, records }
    }

    /// Encode helper: a typed value with an empty unknown bucket.
    #[must_use]
    pub fn from_value(value: T) -> Self {
        Self { value, unknown_fields: Vec::new(), records: BTreeMap::new() }
    }

    /// Fetch a leaf by dotted path and decode it as `U`. The lookup
    /// hash is `field_hash(name, U::SCHEMA)`, so a name match with a
    /// type mismatch returns `None`.
    pub fn get<U: Schema + Body>(&self, name: &str) -> Option<Result<U, Error>> {
        let hash = hash_field(name, &U::SCHEMA);
        let bytes = self.records.get(&hash)?;
        Some(decode_leaf(bytes))
    }

    /// Raw lookup by name and requested type. Same hash as [`Self::get`];
    /// returns the hash and body without decoding.
    #[must_use]
    pub fn get_raw<U: Schema>(&self, name: &str) -> Option<(u64, &[u8])> {
        let hash = hash_field(name, &U::SCHEMA);
        self.records.get(&hash).map(|bytes| (hash, bytes.as_slice()))
    }
}

/// Decode used by derived `Storage` impls.
pub fn decode_derived<T: Leaves>(bytes: &[u8], strict: bool) -> Result<StorageData<T>, Error> {
    let records = records_from_bytes(bytes)?;
    let mut source = Reader::parse(bytes)?;
    let value = T::assemble(field_path_root(), 0, &mut source)?;
    let unknown_fields = if strict {
        source.reject_remaining()?;
        Vec::new()
    } else {
        source.into_unknown()
    };
    Ok(StorageData::from_parts(value, unknown_fields, records))
}

/// Encode used by derived `Storage` impls.
pub fn encode_derived<T: Leaves>(data: &StorageData<T>) -> Result<Vec<u8>, Error> {
    let mut sink = Writer::new();
    data.value.contribute(field_path_root(), 0, &mut sink)?;
    sink.merge_unknown(&data.unknown_fields)?;
    sink.finish()
}

fn records_from_bytes(bytes: &[u8]) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
    let reader = Reader::parse(bytes)?;
    Ok(reader.into_unknown().into_iter().map(|unknown| (unknown.hash, unknown.bytes)).collect())
}

#[cfg(test)]
mod tests;
