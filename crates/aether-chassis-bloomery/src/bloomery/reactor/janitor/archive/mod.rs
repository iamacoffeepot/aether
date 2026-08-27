//! The explicit between-blooms archive pass (ADR-0211).
//!
//! Records — evidence directories and resolved session trees — are never
//! deleted. This pass moves them onto the archive tier when the operator asks,
//! and only when nothing walks. On 2026-08-25 the janitor reclaimed session
//! trees of members still walking (board-5435; dispatches 3301/3318); a later
//! refine lap resumed into a fresh checkout and declined a phantom empty diff.
//! Old trees are records of how the work was figured out.

mod pass;
mod tier;

pub use pass::{ArchiveFailure, ArchiveOutcome, ArchiveRequest, archive_pass};
pub use tier::{ArchiveTier, ArchivedRecord};

#[cfg(test)]
mod tests;
