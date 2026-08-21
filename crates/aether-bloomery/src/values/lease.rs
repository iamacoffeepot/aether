//! What a construct lane's working tree says it has written (ADR-0204).
//!
//! Exclusivity between co-sealed members is per file and acquired at first
//! observed write, so the executor's observation of a slot checkout is the
//! input the lease table is built from. The lane is an untrusted worker and
//! `git status` output is whatever the child left on disk, so
//! [`normalize_write_paths`] is where that observation stops being a claim —
//! the same trust boundary [`SurfaceRequest::normalize`](super::SurfaceRequest)
//! draws for a declining lane's request.

use alloc::borrow::ToOwned as _;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::surface::literal_repo_path;
use crate::ids::WorkpieceId;

/// One later-canonical member whose lane an earlier one's observed write
/// stopped, and the path that did it (ADR-0204).
///
/// The path travels with the member because the eviction record is what an
/// operator reads to learn *why* a lane stopped, and "member B is waiting
/// behind member A" without naming the file is the same information loss the
/// decision exists to remove.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct EvictedHolder {
    /// The member whose lane is cancelled and which re-dispatches once the
    /// evicting member integrates.
    pub workpiece: WorkpieceId,
    /// The contended path.
    pub path: String,
}

/// The per-observation path ceiling. The measured median member write set is
/// 7 files and the measured maximum is 83 (ADR-0204 §Context), so this is far
/// above any honest lane; it exists so a runaway child cannot make one
/// observation unbounded.
pub const MAX_OBSERVED_WRITES: usize = 512;

/// The repository-relative paths one observation of a lane's working tree
/// reports, deduplicated, sorted, and capped.
///
/// Anything that is not one literal repository-relative path is dropped rather
/// than refused: an observation is a best-effort read of a directory a child
/// process owns, and losing the whole observation over one unparseable entry
/// would lose every lease the honest entries would have taken. Sorted because
/// contention resolves by canonical order and a deterministic path order makes
/// the emitted decisions deterministic too.
#[must_use]
pub fn normalize_write_paths(observed: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut paths: Vec<String> =
        observed.into_iter().map(|path| path.trim().to_owned()).filter(|path| literal_repo_path(path)).collect();
    paths.sort();
    paths.dedup();
    paths.truncate(MAX_OBSERVED_WRITES);
    paths
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString as _;
    use alloc::vec;

    use super::{MAX_OBSERVED_WRITES, normalize_write_paths};

    #[test]
    fn an_observation_drops_what_is_not_a_repository_path() {
        // The plausible bug: an absolute or escaping path reaches the lease
        // table, so one member leases a file outside the repository and every
        // sibling that writes it is evicted for a path no bloom owns.
        let observed = vec![
            "crates/a/src/lib.rs".to_string(),
            "/etc/passwd".to_string(),
            "../outside.rs".to_string(),
            "  crates/a/src/lib.rs  ".to_string(),
            String::new(),
        ];

        assert_eq!(normalize_write_paths(observed), vec!["crates/a/src/lib.rs".to_string()]);
    }

    #[test]
    fn an_observation_is_capped() {
        let observed = (0..MAX_OBSERVED_WRITES + 32).map(|index| alloc::format!("crates/a/src/f{index}.rs"));

        assert_eq!(normalize_write_paths(observed).len(), MAX_OBSERVED_WRITES);
    }
}
