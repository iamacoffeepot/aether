//! Append-only, single-writer log of typed events, backed by SQLite.
//!
//! The journal records [`aether_data::Storage`] kinds and content-addressed
//! artifacts. It never deletes, rewrites, reorders, compacts, migrates, folds
//! views, or runs reactors. See ADR-0220.

mod artifact;
mod clock;
mod draft;
mod entry;
mod journal;

pub use artifact::Digest;
pub use clock::{Clock, SystemClock};
pub use draft::{Draft, DraftError};
pub use entry::{Entry, Seq};
pub use journal::{AppendError, DecodeError, Journal, JournalError};
