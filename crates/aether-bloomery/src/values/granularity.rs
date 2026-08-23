//! Granularity: the shape a requested path has to take before the seal door
//! admits it, and the widened surface a successor revision declares.
//!
//! Shared by the two halves that widen a surface, so they cannot disagree about
//! what a request is admitted as: `xtask bloom amend`, which an operator drives,
//! and the coordinator's own auto-tier grant (ADR-0207), which no one drives.
//!
//! A declared-surface entry that names one file is admitted only when a
//! file-granular approval-policy rule names that same file. A blocked lane asks
//! for the file it stopped on, which is the honest thing for it to say and the
//! wrong thing to seal, so the amendment widens the ask to the glob that covers
//! it and reports both spellings.
//!
//! The policy stays the authority on which files are worth naming: the same
//! `unnamed_file_entries` the seal door refuses on is what picks the entries to
//! widen, so there is no second table here to drift from the sealed one.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::approval::{ApprovalPolicy, surface_additions};

/// The surface the successor will declare, and how it was arrived at.
///
/// One value rather than five returns, because every field is a projection of
/// the same rewrite and computing them apart invites two of them to disagree
/// about what the amendment granted.
pub struct Widening {
    /// The current revision's surface, at the granularity the seal admits.
    pub existing: Vec<String>,
    /// The globs [`Widening::existing`] does not already permit.
    pub added: Vec<String>,
    /// `existing` followed by `added` — what the successor declares.
    pub widened: Vec<String>,
    /// Requested paths the policy does not name, and the glob each is admitted
    /// as, so the printed plan shows the ask beside the grant.
    pub coarsened: Vec<(String, String)>,
    /// The same rewrite over entries the current revision already carries.
    /// Not additions, but they are why the successor revision can differ from
    /// the tip without anything being added.
    pub inherited: Vec<(String, String)>,
}

/// Widen `current` by `requested`, both at the granularity the seal admits.
///
/// The current surface is coarsened as well as the request, because a revision
/// sealed before the request arrived can already carry a raw file entry:
/// appending a covering glob beside it would leave in place the very entry the
/// seal door refuses on, and the successor would be refused for the reason its
/// predecessor was.
///
/// # Errors
/// The first requested glob outside the declared-surface grammar, by name. The
/// request comes from an untrusted lane, so an unparseable glob is refused
/// rather than skipped.
pub fn widen(policy: &ApprovalPolicy, current: &[String], requested: &[String]) -> Result<Widening, String> {
    let existing = coarsen(policy, current);
    let added = surface_additions(&existing, &coarsen(policy, requested))?;

    let widened = existing.iter().chain(added.iter()).cloned().collect();
    Ok(Widening {
        coarsened: rewrites(policy, requested),
        inherited: rewrites(policy, current),
        existing,
        added,
        widened,
    })
}

/// The surface `entries` are admitted as: every entry naming a file the policy
/// does not name replaced by the glob covering it, in order, deduplicated.
///
/// Widening, not narrowing — the glob covers every path the entry named — so an
/// entry the tip already carries may be rewritten without weakening what the
/// existing approval was read against.
#[must_use]
pub fn coarsen(policy: &ApprovalPolicy, entries: &[String]) -> Vec<String> {
    let unnamed = policy.unnamed_file_entries(entries);
    let mut admitted: Vec<String> = Vec::new();
    for entry in entries {
        let glob = if unnamed.contains(entry) {
            covering_glob(entry).unwrap_or_else(|| entry.clone())
        } else {
            entry.clone()
        };
        if !admitted.contains(&glob) {
            admitted.push(glob);
        }
    }
    admitted
}

/// The entries [`coarsen`] rewrites, paired with what it rewrites them to, so
/// the printed plan can show the operator the ask and the grant side by side.
#[must_use]
pub fn rewrites(policy: &ApprovalPolicy, entries: &[String]) -> Vec<(String, String)> {
    policy
        .unnamed_file_entries(entries)
        .into_iter()
        .filter_map(|entry| covering_glob(&entry).map(|glob| (entry, glob)))
        .collect()
}

/// The glob a file path is admitted as, or `None` for a path with no tree to
/// widen to — a repository-root file, whose only way in is the policy naming
/// it.
///
/// `crates`, `docs` and `.github` carry two segments because their first
/// segment alone is the whole repository's worth of one kind of thing;
/// everything else widens to the top-level directory it lives in.
fn covering_glob(path: &str) -> Option<String> {
    let (head, rest) = path.split_once('/')?;
    if matches!(head, "crates" | "docs" | ".github") {
        let (name, _) = rest.split_once('/')?;
        return Some(format!("{head}/{name}/**"));
    }
    Some(format!("{head}/**"))
}
