//! The operator archive pass: move eligible records onto the tier, or refuse
//! the whole pass if anything still walks.
//!
//! On 2026-08-25 the janitor reclaimed session trees of members still walking
//! (board-5435; dispatches 3301/3318); a later refine lap resumed into a
//! fresh checkout and declined a phantom empty diff. Old trees are records of
//! how the work was figured out. This pass is explicit and gated on the same
//! between-blooms predicate the sweep already computes, so a move never races
//! a session that can still resume.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use aether_bloomery::{BloomId, Snapshot};

use crate::bloomery::TransformRunner;
use crate::store::StoreBackend;
use crate::store::membership;

use super::super::JanitorPolicy;
use super::super::records::{
    EVIDENCE_SUFFIX, SESSIONS_DIR, age_days, bloom_of, dispatch_owners, evidence_is_protected, evidence_nonce_of,
    live_session_slugs, session_slug_of, walking_reason,
};
use super::tier::{ArchiveError, ArchiveTier, ArchivedRecord, RecordClass};

/// The seams one archive pass reads and writes.
pub struct ArchiveRequest<'a> {
    /// The journal the snapshot is rebuilt from, and the outstanding-order /
    /// session-owner / commission tables the disk walk is keyed by.
    pub store: &'a mut dyn StoreBackend,
    /// The spawn seam that lists registered worktrees and drops them after a
    /// session tree has already moved.
    pub runner: &'a dyn TransformRunner,
    /// Scratch-worktree base: nonce-keyed checkouts and `*-evidence` dirs.
    pub worktree_base: &'a Path,
    /// Where eligible records move.
    pub tier: &'a ArchiveTier,
    /// Retention window for consumed evidence.
    pub policy: &'a JanitorPolicy,
    /// The clock evidence age is measured against.
    pub now: SystemTime,
}

/// What one pass did, or why it touched nothing.
#[derive(Debug)]
pub enum ArchiveOutcome {
    /// Eligible records moved, plus per-record failures that left their source
    /// in place.
    Archived {
        /// Records that now live on the tier.
        records: Vec<ArchivedRecord>,
        /// Records that could not move.
        failures: Vec<ArchiveFailure>,
    },
    /// The coordinator is not between blooms. Nothing moved.
    Refused {
        /// The walking bloom or outstanding nonce.
        reason: String,
    },
}

/// Why one record in an otherwise successful pass did not move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveFailure {
    /// `evidence` or `session`.
    pub class: String,
    /// The name the record was addressed by.
    pub name: String,
    /// Why the move did not complete.
    pub error: String,
}

/// Replay the membership snapshot, refuse if anything walks, otherwise archive
/// every eligible evidence directory and resolved session tree.
///
/// A session tree is moved before its worktree registration is dropped, so the
/// registration is pruned against an already-absent path rather than the git
/// removal deleting the tree. A record whose move failed is reported and left
/// registered. Nothing in this function removes a record directory.
///
/// # Errors
/// The store could not be read.
pub fn archive_pass(request: &mut ArchiveRequest<'_>) -> rusqlite::Result<ArchiveOutcome> {
    let snapshot = membership::replay_snapshot(request.store)?;
    let outstanding = request.store.list_outstanding_nonces()?;
    if let Some(reason) = walking_reason(&snapshot, &outstanding) {
        return Ok(ArchiveOutcome::Refused { reason });
    }

    let live: HashSet<&str> = outstanding.iter().map(String::as_str).collect();
    let owners = dispatch_owners(request.store, &outstanding)?;
    let mut records = Vec::new();
    let mut failures = Vec::new();

    archive_evidence(request, &live, &owners, &snapshot, &mut records, &mut failures);
    archive_session_trees(request, &snapshot, &outstanding, &mut records, &mut failures)?;

    Ok(ArchiveOutcome::Archived { records, failures })
}

fn archive_evidence(
    request: &mut ArchiveRequest<'_>,
    live: &HashSet<&str>,
    owners: &HashMap<String, BloomId>,
    snapshot: &Snapshot,
    records: &mut Vec<ArchivedRecord>,
    failures: &mut Vec<ArchiveFailure>,
) {
    let Ok(entries) = fs::read_dir(request.worktree_base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(nonce) = evidence_nonce_of(&path) else {
            continue;
        };
        let owner = bloom_of(request.store, &nonce, owners);
        if evidence_is_protected(live, snapshot, &nonce, owner.as_ref()) {
            continue;
        }
        if age_days(&path, request.now).is_none_or(|age| age < request.policy.evidence_retention_days) {
            continue;
        }
        let name = format!("{nonce}{EVIDENCE_SUFFIX}");
        push_result(
            request.tier.archive(RecordClass::Evidence, &name, &path),
            records,
            failures,
            RecordClass::Evidence,
            &name,
        );
    }
}

fn archive_session_trees(
    request: &mut ArchiveRequest<'_>,
    snapshot: &Snapshot,
    outstanding: &[String],
    records: &mut Vec<ArchivedRecord>,
    failures: &mut Vec<ArchiveFailure>,
) -> rusqlite::Result<()> {
    let Ok(base) = fs::canonicalize(request.worktree_base.join(SESSIONS_DIR)) else {
        return Ok(());
    };
    let registered = match request.runner.registered_worktrees() {
        Ok(registered) => registered,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                %error,
                "janitor: worktree registrations unreadable; session trees not archived",
            );
            return Ok(());
        }
    };
    let live = live_session_slugs(request.store, snapshot, outstanding)?;

    for worktree in registered {
        let Some(slug) = session_slug_of(&base, &worktree) else {
            continue;
        };
        if live.contains(slug.as_str()) {
            continue;
        }
        if !session_commission_resolved(request.store, &slug)? {
            continue;
        }
        match request.tier.archive(RecordClass::Session, &slug, &worktree) {
            Ok(record) => {
                drop_registration(request.runner, &worktree);
                records.push(record);
            }
            Err(error) => failures.push(failure(RecordClass::Session, &slug, &error)),
        }
    }
    Ok(())
}

/// A slug with no owner row, or an owner whose commission is neither landed
/// nor cancelled, is not eligible — unknown reads as live, the way
/// `successor_chain_is_terminal` treats an unresolvable end.
fn session_commission_resolved(store: &mut dyn StoreBackend, slug: &str) -> rusqlite::Result<bool> {
    let Some((_bloom, workpiece)) = store.lookup_session_owner(slug)? else {
        return Ok(false);
    };
    store.commission_is_resolved(&workpiece)
}

fn drop_registration(runner: &dyn TransformRunner, path: &Path) {
    if let Err(error) = runner.release(path) {
        tracing::warn!(
            target: "aether_chassis_bloomery::janitor",
            worktree = %path.display(),
            %error,
            "janitor: worktree registration drop after archive failed; the tree has already moved",
        );
    }
}

fn push_result(
    result: Result<ArchivedRecord, ArchiveError>,
    records: &mut Vec<ArchivedRecord>,
    failures: &mut Vec<ArchiveFailure>,
    class: RecordClass,
    name: &str,
) {
    match result {
        Ok(record) => records.push(record),
        Err(error) => failures.push(failure(class, name, &error)),
    }
}

fn failure(class: RecordClass, name: &str, error: &ArchiveError) -> ArchiveFailure {
    ArchiveFailure { class: class.as_str().to_owned(), name: name.to_owned(), error: error.to_string() }
}
