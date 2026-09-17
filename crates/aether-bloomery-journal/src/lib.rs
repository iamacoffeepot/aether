//! Append-only, single-writer log of typed events, backed by `SQLite`.
//!
//! Two layers, and the split is the point:
//!
//! - **The store stays raw.** The `artifacts` table is content-addressed
//!   (`digest` = `sha256(bytes)`) and knows nothing about kinds.
//! - **An artifact is an abstraction above the store:** a blob whose bytes
//!   are an eight-byte [`aether_data::KindId`] prefix followed by a payload.
//!   The digest covers the kind, so a digest names one kind and one payload.
//!
//! Events are [`aether_data::Storage`] kinds. The only write is a [`Batch`] of
//! staged artifacts plus events; [`Journal::append`] and the fenced
//! [`Journal::prepare_append`] / [`Journal::commit_prepared`] pair are the only
//! write routes.
//! Citations are typed [`Ref`] values collected by a derive-emitted walk.
//! The one recognized exception is `bloomery.head_moved`: `append` decodes
//! that kind from the draft as [`aether_bloomery_kinds::RecordedHeadMove`]
//! and verifies that its destination exists with the recorded head's
//! eight-byte prefix. A missing citation walk cannot stand in for that
//! check. See ADR-0220.
//!
//! [`JournalIdentity`] is a process-local allocation token minted by each
//! constructor so a view registry can detect replacement. It is not persisted
//! and is not a SQL column.

mod artifact;
mod batch;
mod clock;
mod draft;
mod journal;

pub use aether_bloomery_kinds::{
    DecodeError, Digest, Entry, OpaqueBytes, Ref, Seq, Utf8Text, artifact_blob, artifact_digest, artifact_prefix,
    hash_bytes,
};
pub use artifact::split_artifact;
pub use batch::{Batch, BatchError};
pub use clock::{Clock, SystemClock};
pub use draft::{Draft, DraftError};
pub use journal::{AppendError, GetError, Journal, JournalError, JournalIdentity, PreparedAppend};
