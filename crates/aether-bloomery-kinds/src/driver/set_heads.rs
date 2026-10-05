//! Compatibility decoding for the original single-head intent.

use alloc::vec;

use aether_data::{Digest, Kind, KindId};

use crate::{HeadChange, RecordedHead, SetHeads};

#[aether_data::kind(name = "aether.bloomery.driver.set_head", eq, no_serde)]
struct LegacySetHead {
    head: RecordedHead,
    from: Option<Digest>,
    to: Digest,
}

/// Pinned kind id for the original single-head wire generation.
pub const LEGACY_SET_HEAD_ID: KindId = LegacySetHead::ID;

/// Decode either head-setting wire generation selected by its exact kind id.
///
/// The kind id chooses exactly one schema; malformed bytes never fall back
/// to the other generation.
#[must_use]
pub fn decode_set_heads(kind: KindId, bytes: &[u8]) -> Option<SetHeads> {
    if kind == SetHeads::ID {
        return SetHeads::decode_from_bytes(bytes);
    }
    if kind == LegacySetHead::ID {
        return LegacySetHead::decode_from_bytes(bytes)
            .map(|legacy| SetHeads::new(vec![HeadChange::from_recorded(legacy.head, legacy.from, legacy.to)]));
    }
    None
}
