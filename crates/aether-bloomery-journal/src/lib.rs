//! Append-only, single-writer log of typed events over one journal root.
//!
//! A root is a directory holding `journal.sqlite` (the event log and one
//! row per stored artifact) and `blobs/<first two hex>/<digest hex>`, one
//! file per artifact holding exactly the bytes its digest hashes. Each blob
//! file is written temp file, fsync, rename, directory fsync before the row
//! that names it commits. [`Journal::open`] takes an exclusive lock on the
//! root, so a second open fails in any process, and sweeps `blobs/tmp/`.
//! [`JournalReader`] observes a root without the lock. See ADR-0220.
//!
//! Two layers, and the split is the point:
//!
//! - **The store stays raw.** Artifacts are content-addressed
//!   (`digest` = `sha256(bytes)`) and the store knows nothing about kinds.
//! - **An artifact is an abstraction above the store:** a blob whose bytes
//!   are an eight-byte [`aether_data::KindId`] prefix followed by a payload.
//!   The digest covers the kind, so a digest names one kind and one payload.
//!
//! Events are [`aether_data::Storage`] kinds. The only event write is a
//! [`Batch`] of staged artifacts plus events; [`Journal::append`] judges it.
//! Artifacts also land through a second door over the same root lock: an
//! [`ArtifactStore`] derived by [`Journal::artifact_store`] opens
//! [`ArtifactBatch`]es on any thread, which stream blobs into their files a
//! chunk at a time ([`BlobFile`]) and commit their rows through the same row
//! insert and citation check `append` runs (ADR-0237 open question 1).
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
//!
//! [`JournalActor`] is the native owner for one named journal root. It answers
//! read, head, and artifact mail plus three fenced writes while keeping its
//! journal handle inside the actor: [`aether_bloomery_kinds::MoveHead`] moves
//! one head to an already-stored artifact, [`aether_bloomery_kinds::Publish`]
//! stages encoded artifacts and appends head moves in one atomic batch, and
//! [`aether_bloomery_kinds::AppendRecords`] stages artifacts and appends the
//! native bundle driver's own records and head moves, each under its own
//! cause. All three fence on the whole journal's last stored sequence;
//! `MoveHead` / `Publish` carry no cause, and `AppendRecords` is the one
//! write that does. It also answers [`aether_bloomery_kinds::WatchHead`], a
//! bounded long poll that is answered once a committed write moves the head
//! past `after`, and [`aether_bloomery_kinds::ReadClosure`], a read of an
//! artifact's transitive closure under a validated byte limit.
//! [`aether_bloomery_kinds::ReadArtifact`] and `ReadClosure` both run off the
//! actor's thread, each on its own task queue (ADR-0093), so a large artifact
//! or closure never holds up the actor's other requests. Both reuse members
//! the journal still holds, under the [`ReadCacheBudget`], and read only the
//! misses.
//!
//! A blob stored for the first time records its citation edges in the
//! `citations` table inside the same append transaction, so the edges are
//! fixed when the blob is, and a later re-staging with a different citation
//! list cannot change them. [`Journal::read_closure`] walks those edges
//! breadth-first under a byte budget and never truncates (ADR-0226
//! decision 10). Artifacts stored before the table existed have no edges.

mod actor;
mod artifact;
mod batch;
mod blobs;
mod cache;
mod clock;
mod closure;
mod draft;
mod journal;
mod reader;
mod store;
mod watch;
mod worker;

pub use actor::{JournalActor, MAX_HEAD_WATCHERS, MAX_READ_EVENTS};
pub use aether_bloomery_kinds::{
    ArtifactHasher, DecodeError, Digest, Entry, OpaqueBytes, Ref, Seq, Utf8Text, artifact_blob, artifact_digest,
    artifact_prefix, hash_bytes,
};
pub use artifact::split_artifact;
pub use batch::{Batch, BatchError};
pub use cache::ReadCacheBudget;
pub use clock::{Clock, SystemClock};
pub use closure::Closure;
pub use draft::{Draft, DraftError};
pub use journal::{AppendError, GetError, Journal, JournalError, JournalIdentity};
pub use reader::JournalReader;
pub use store::{ArtifactBatch, ArtifactStore, BlobFile, VerifiedBlob};
