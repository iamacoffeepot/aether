//! Tripwire: a `Ref<K>` field encodes like a `[u8; 32]` field.

use aether_bloomery_journal::{Digest, OpaqueBytes, Ref};
use aether_data::{Storage, StorageData};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.with_ref")]
struct WithRef {
    digest: Ref<OpaqueBytes>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.with_array")]
struct WithArray {
    digest: [u8; 32],
}

#[test]
fn a_ref_field_encodes_byte_identically_to_a_byte_array_field() {
    // Tripwire: terminate_field_hash folds the path carry and the canonical
    // schema bytes only (crates/aether-data/src/storage/hash.rs);
    // Schema::LABEL is not in the preimage, so replacing a Digest field with
    // a Ref<K> field must not move the field tag and must not change the body.
    // If it ever does, every entry already written decodes as a missing
    // required field.
    let bytes = [7u8; 32];
    let with_ref = WithRef { digest: Ref::from_digest(Digest::from_bytes(bytes)) };
    let with_array = WithArray { digest: bytes };
    let ref_bytes = WithRef::encode_storage(&StorageData::from_value(with_ref)).expect("encode ref");
    let array_bytes = WithArray::encode_storage(&StorageData::from_value(with_array)).expect("encode array");
    assert_eq!(ref_bytes, array_bytes);
}
