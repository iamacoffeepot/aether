//! Resolve on send (ADR-0238 decision 3, amended by #6751).
//!
//! A payload can already carry tag-1 `Blob` fields before it is sent: a
//! native actor raw-forwards bytes it received, or a guest encodes a `Blob`
//! it holds as its hash. Before such a send leaves its sender, the engine
//! walks the kind's schema, finds each tag-1 hash, and attaches the entry the
//! sender itself holds under that hash. The lookup sees only the sender's own
//! blobs (a guest's table pins and holds, or the attachments of the mail a
//! native actor is handling), so a guessed or logged hash reaches nothing the
//! sender does not already hold (decision 4), and a hash that resolves
//! nowhere refuses the send at the sender.
//!
//! A sender that holds no blob cannot carry a valid tag-1 field, so callers
//! run this only for senders that hold blobs: blob-free senders pay no schema
//! lookup and no walk.

use std::fmt;
use std::sync::Arc;

use aether_codec::{DecodeError, InlineError, blob_hashes};
use aether_data::{BlobHash, KindId};

use super::Attachments;
use crate::mail::Registry;
use crate::store::BlobEntry;

/// Why [`resolve_on_send`] refused a payload.
#[derive(Debug)]
pub enum ResolveError {
    /// A tag-1 hash the sender neither holds nor pins.
    Unresolved(BlobHash),
    /// The payload does not match its kind's schema.
    Malformed(DecodeError),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolved(hash) => {
                f.write_str("tag-1 blob field names hash ")?;
                for byte in hash.as_bytes() {
                    write!(f, "{byte:02x}")?;
                }
                f.write_str(", which the sender does not hold")
            }
            Self::Malformed(error) => write!(f, "payload does not match its schema: {error}"),
        }
    }
}

/// Resolve on send: the attachments for the tag-1 hashes already in
/// `payload`, a `kind` mail's bytes, one per distinct hash, each found by
/// `own`.
///
/// A kind the registry does not know returns `Ok(None)`: no decoder can read a
/// `Blob` from a kind without a schema. So does a payload with no tag-1 field.
///
/// # Errors
///
/// [`ResolveError::Unresolved`] for the first hash `own` does not find, and
/// [`ResolveError::Malformed`] for a payload that does not follow the kind's
/// schema.
pub fn resolve_on_send(
    registry: &Registry,
    kind: KindId,
    payload: &[u8],
    own: impl Fn(BlobHash) -> Option<Arc<BlobEntry>>,
) -> Result<Attachments, ResolveError> {
    let Some(descriptor) = registry.kind_descriptor(kind) else {
        return Ok(None);
    };
    let hashes = blob_hashes(&descriptor.schema, payload).map_err(|error| match error {
        InlineError::Malformed(error) => ResolveError::Malformed(error),
        // `blob_hashes` pairs no hash with an attachment and sizes nothing,
        // so neither arm arises; each still maps to the refusal it names.
        InlineError::MissingAttachment { hash } => ResolveError::Unresolved(hash),
        InlineError::TooLarge { .. } => {
            ResolveError::Malformed(DecodeError::UnsupportedSchema("blob hash listing reported a size"))
        }
    })?;

    let mut entries: Vec<Arc<BlobEntry>> = Vec::new();
    for hash in hashes {
        if entries.iter().any(|entry| entry.hash() == hash) {
            continue;
        }
        entries.push(own(hash).ok_or(ResolveError::Unresolved(hash))?);
    }
    Ok((!entries.is_empty()).then(|| entries.into_boxed_slice()))
}
