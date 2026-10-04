//! The two prefix-only leaf kinds: an artifact whose payload is raw bytes or
//! UTF-8 text rather than an encoded kind.

use crate::{Kind, KindId};

/// Arbitrary bytes, no promise. Never instantiated; names a prefix.
pub struct OpaqueBytes;

impl Kind for OpaqueBytes {
    const NAME: &'static str = "aether.artifact.bytes";
    const ID: KindId = crate::storage_kind_id_from_name(Self::NAME);
}

/// UTF-8 text, validated when staged. Never instantiated; names a prefix.
pub struct Utf8Text;

impl Kind for Utf8Text {
    const NAME: &'static str = "aether.artifact.text";
    const ID: KindId = crate::storage_kind_id_from_name(Self::NAME);
}
