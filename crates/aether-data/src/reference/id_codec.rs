//! Shared id codec for the proven reference types: every door in rejects
//! any `u64` whose tag bits are not `Tag::Mailbox`, which also rejects zero,
//! so `MailboxId::NONE` is unrepresentable as a reference.

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serializer};

use crate::MailboxId;
use crate::ids::{deserialize_id, serialize_id};
use crate::tagged_id::{self, Tag};
use crate::wire::{Error as WireError, WireDecode};

/// Read eight bytes and accept them as a proven id only when the tag bits
/// are `Tag::Mailbox`. Anything else, including zero, is
/// [`InvalidReference`](WireError::InvalidReference) carrying the raw value.
pub fn decode_reference_id(cursor: &mut &[u8]) -> Result<MailboxId, WireError> {
    let raw = u64::decode(cursor)?;
    if tagged_id::tag_of(raw) == Some(Tag::Mailbox) {
        Ok(MailboxId(raw))
    } else {
        Err(WireError::InvalidReference(raw))
    }
}

/// Serialize a proven id exactly like [`MailboxId`]: a tagged `mbx-` string
/// in human-readable formats, a bare `u64` otherwise. A proven id always
/// carries `Tag::Mailbox` bits, so the string branch always applies.
pub fn serialize_reference_id<S: Serializer>(id: MailboxId, serializer: S) -> Result<S::Ok, S::Error> {
    serialize_id(id.0, serializer)
}

/// Deserialize a proven id like [`MailboxId`], with the tag check on the
/// binary path: a raw `u64` is accepted only with `Tag::Mailbox` bits. The
/// human-readable path takes `deserialize_id` as is — a tagged string
/// enforces the `mbx-` tag there, and a bare number stays the back-compat
/// spelling.
pub fn deserialize_reference_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<MailboxId, D::Error> {
    if deserializer.is_human_readable() {
        deserialize_id(deserializer, Tag::Mailbox).map(MailboxId)
    } else {
        let raw = u64::deserialize(deserializer)?;
        if tagged_id::tag_of(raw) == Some(Tag::Mailbox) {
            Ok(MailboxId(raw))
        } else {
            Err(DeError::custom("invalid actor reference id"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_zero() {
        let bytes = 0u64.to_le_bytes();
        let mut cursor: &[u8] = &bytes;
        assert_eq!(decode_reference_id(&mut cursor), Err(WireError::InvalidReference(0)));
    }

    #[test]
    fn decode_rejects_wrong_tag() {
        let raw = tagged_id::with_tag(Tag::Kind, 1);
        let bytes = raw.to_le_bytes();
        let mut cursor: &[u8] = &bytes;
        assert_eq!(decode_reference_id(&mut cursor), Err(WireError::InvalidReference(raw)));
    }
}
