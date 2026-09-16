//! Portable journal entry envelopes, with an optional SQLite-backed store.
//!
//! Without default features this crate is `no_std` + `alloc` and exposes
//! [`Entry`], [`Seq`], [`DecodeError`], and [`Entry::decode`]. The default
//! `sqlite` feature is the native append-only, single-writer log of typed
//! events, backed by `SQLite`.
//!
//! Two layers, and the split is the point:
//!
//! - **The store stays raw.** The `artifacts` table is content-addressed
//!   (`digest` = `sha256(bytes)`) and knows nothing about kinds.
//! - **An artifact is an abstraction above the store:** a blob whose bytes
//!   are an eight-byte [`aether_data::KindId`] prefix followed by a payload.
//!   The digest covers the kind, so a digest names one kind and one payload.
//!
//! With `sqlite`, events are [`aether_data::Storage`] kinds. The only write is a
//! `Batch` of staged artifacts plus events; `Journal::append` is the only judge.
//! Citations are typed [`Ref`] values collected by a derive-emitted walk.
//! The one recognized exception is `bloomery.head_moved`: `append` decodes
//! that kind from the draft as [`aether_bloomery_kinds::RecordedHeadMove`]
//! and verifies that its destination exists with the recorded head's
//! eight-byte prefix. A missing citation walk cannot stand in for that
//! check. See ADR-0220.
//!
//! With `sqlite`, `JournalIdentity` is a process-local allocation token minted by each
//! constructor so a view registry can detect replacement. It is not persisted
//! and is not a SQL column.

#![cfg_attr(not(feature = "sqlite"), no_std)]

extern crate alloc;

#[cfg(feature = "sqlite")]
mod artifact;
#[cfg(feature = "sqlite")]
mod batch;
#[cfg(feature = "sqlite")]
mod clock;
#[cfg(feature = "sqlite")]
mod draft;
mod entry;
#[cfg(feature = "sqlite")]
mod journal;

pub use aether_bloomery_kinds::{
    Digest, OpaqueBytes, Ref, Utf8Text, artifact_blob, artifact_digest, artifact_prefix, hash_bytes,
};
#[cfg(feature = "sqlite")]
pub use artifact::split_artifact;
#[cfg(feature = "sqlite")]
pub use batch::{Batch, BatchError};
#[cfg(feature = "sqlite")]
pub use clock::{Clock, SystemClock};
#[cfg(feature = "sqlite")]
pub use draft::{Draft, DraftError};
pub use entry::{DecodeError, Entry, Seq};
#[cfg(feature = "sqlite")]
pub use journal::{AppendError, GetError, Journal, JournalError, JournalIdentity};
