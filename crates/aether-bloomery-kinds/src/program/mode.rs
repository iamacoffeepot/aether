//! Purity of a program: the same input digest always yields the same result, or not.

use aether_data::storage::{
    RecordReader, RecordWriter, StorageError, UNIT_SCHEMA, VARIANT_LEAF, fold_path_segment, variant_hash,
};
use aether_data::{Citations, Cites, StorageLeaves};

/// How executions of a program relate across executors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Schema)]
pub enum Mode {
    /// Every correct executor produces the same result digest for the same input digest.
    Pure,
    /// Each execution is one observation. Never memoized.
    Sampled,
}

impl Cites for Mode {
    fn cites(&self, _sink: &mut Citations) {}
}

impl StorageLeaves for Mode {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        let disc = match self {
            Self::Pure => variant_hash("Pure", &UNIT_SCHEMA),
            Self::Sampled => variant_hash("Sampled", &UNIT_SCHEMA),
        };
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        u64::contribute(&disc, var_carry, depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        let disc = u64::assemble(var_carry, depth + 1, source)?;
        if disc == variant_hash("Pure", &UNIT_SCHEMA) {
            Ok(Self::Pure)
        } else if disc == variant_hash("Sampled", &UNIT_SCHEMA) {
            Ok(Self::Sampled)
        } else {
            Err(StorageError::UnknownVariant { hash: disc })
        }
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        u64::is_absent(var_carry, depth + 1, source)
    }
}
