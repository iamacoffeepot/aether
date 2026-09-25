//! The store entries an in-process envelope carries beside its payload
//! (ADR-0238 decision 3), and the rewrite that takes them out of the process.
//!
//! In-process mail writes a `Blob` field as tag 1 and the blob's hash, and the
//! envelope attaches the store entry that hash names, so the recipient shares
//! the bytes instead of copying them. The attachments ride every hand-off of
//! the envelope: [`Mail`](super::Mail), the pending send buffered until flush,
//! and the [`OwnedDispatch`](super::OwnedDispatch) an actor's inbox receives.
//! Each holds its entries as strong references, so the bytes stay resident
//! until the last holder drops.
//!
//! A tag-1 field is written only beside an attachment, so an envelope without
//! attachments has no tag-1 field. Every consumer that takes plain payload
//! bytes checks for attachments first and uses the bytes as they are when
//! there are none: blob-free mail pays one empty check and no walk. With
//! attachments, [`inline_payload`] rewrites each tag-1 field to tag 0 through
//! [`aether_codec::inline_blobs`], bounded by the caller's limit.

use std::borrow::Cow;
use std::sync::Arc;

use aether_codec::{DecodeError, InlineError, inline_blobs};
use aether_data::{BlobHash, KindId};

use crate::mail::Registry;
use crate::store::BlobEntry;

#[cfg(any(test, feature = "test-support"))]
mod sharing;
#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
pub use sharing::SharingEncoder;

/// The store entries an in-process envelope's tag-1 `Blob` fields name by
/// hash. `None` when the payload has none.
pub type Attachments = Option<Box<[Arc<BlobEntry>]>>;

/// `attachments` with an empty set folded to `None`, so "has attachments"
/// is one `is_some` wherever the set is read.
pub fn normalized(attachments: Attachments) -> Attachments {
    attachments.filter(|entries| !entries.is_empty())
}

/// `payload`, a `kind` mail's bytes, with each tag-1 `Blob` field rewritten
/// to tag 0 from the entry in `attachments` its hash names. The kind's schema
/// comes from `registry`.
///
/// # Errors
///
/// [`InlineError::TooLarge`] past `limit_bytes`, [`InlineError::MissingAttachment`]
/// for a hash no entry carries, and [`InlineError::Malformed`] for a payload
/// that does not follow the schema or a kind the registry does not hold.
pub fn inline_payload(
    registry: &Registry,
    kind: KindId,
    payload: &[u8],
    attachments: &[Arc<BlobEntry>],
    limit_bytes: usize,
) -> Result<Vec<u8>, InlineError> {
    let descriptor = registry
        .kind_descriptor(kind)
        .ok_or(InlineError::Malformed(DecodeError::UnsupportedSchema("an attached mail's kind is not registered")))?;
    let pairs: Vec<(BlobHash, &[u8])> = attachments.iter().map(|entry| (entry.hash(), entry.bytes())).collect();
    inline_blobs(&descriptor.schema, payload, &pairs, limit_bytes)
}

/// The payload a plain-bytes consumer reads: `payload` itself, borrowed and
/// unwalked, when `attachments` is empty, else [`inline_payload`]'s rewrite.
///
/// # Errors
///
/// As [`inline_payload`], only when `attachments` is not empty.
pub fn plain_payload<'a>(
    registry: &Registry,
    kind: KindId,
    payload: &'a [u8],
    attachments: &[Arc<BlobEntry>],
    limit_bytes: usize,
) -> Result<Cow<'a, [u8]>, InlineError> {
    if attachments.is_empty() {
        return Ok(Cow::Borrowed(payload));
    }
    inline_payload(registry, kind, payload, attachments, limit_bytes).map(Cow::Owned)
}
