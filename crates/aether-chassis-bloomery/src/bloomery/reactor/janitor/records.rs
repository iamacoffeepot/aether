//! Selection helpers shared by the janitor sweep and the archive pass.
//!
//! Eligibility has one definition: both the tick and the operator archive
//! pass read these predicates rather than each recomputing liveness. The
//! predicates themselves do not decide what happens to a matching record.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
#[cfg(test)]
use std::time::Duration;
use std::time::SystemTime;

use aether_bloomery::{BloomId, Digest, Snapshot, is_active_unlanded};

use crate::store::StoreBackend;

/// The suffix a dispatch's evidence directory carries under the scratch root —
/// the same spelling [`LocalExecutor`](crate::bloomery::LocalExecutor) uses.
pub(super) const EVIDENCE_SUFFIX: &str = "-evidence";

/// The directory under the scratch root holding one checkout per reusable
/// harness session, and the working tree inside each — the same spelling
/// [`LocalExecutor`](crate::bloomery::LocalExecutor) lays them out under.
pub(super) const SESSIONS_DIR: &str = "sessions";
pub(super) const SESSION_TREE_DIR: &str = "tree";

/// Seconds in a day — the unit [`super::JanitorPolicy::evidence_retention_days`]
/// is stated in.
pub(super) const SECS_PER_DAY: u64 = 86_400;

/// Whether the coordinator is between blooms: nothing in the replayed snapshot
/// is still walking, and no order is outstanding.
///
/// Tree and text reclaim, and the archive pass, run only then. The live-set
/// derivation that used to decide which session trees were reclaimable is a
/// computed claim racing live work; this gate does not depend on that
/// computation being right.
pub(super) fn between_blooms(snapshot: &Snapshot, outstanding: &[String]) -> bool {
    walking_reason(snapshot, outstanding).is_none()
}

/// Why the coordinator is not between blooms, naming the walking bloom or the
/// outstanding nonce. `None` when nothing walks.
pub(super) fn walking_reason(snapshot: &Snapshot, outstanding: &[String]) -> Option<String> {
    if let Some(nonce) = outstanding.first() {
        return Some(format!("order {nonce} is still outstanding"));
    }
    snapshot
        .blooms
        .iter()
        .find(|(_, record)| is_active_unlanded(record.status))
        .map(|(bloom, _)| format!("bloom {} is still walking", bloom.0.to_hex()))
}

/// Nonce → owning bloom, from outstanding rows and the durable owner table.
pub(super) fn dispatch_owners(
    store: &mut dyn StoreBackend,
    outstanding: &[String],
) -> rusqlite::Result<HashMap<String, BloomId>> {
    let mut owners = HashMap::new();
    for nonce in outstanding {
        if let Some(order) = store.lookup_order(nonce)?
            && let Some(bloom) = Digest::from_slice(&order.bloom).map(BloomId)
        {
            owners.insert(nonce.clone(), bloom);
        }
    }
    Ok(owners)
}

pub(super) fn bloom_of(
    store: &mut dyn StoreBackend,
    nonce: &str,
    owners: &HashMap<String, BloomId>,
) -> Option<BloomId> {
    if let Some(bloom) = owners.get(nonce) {
        return Some(*bloom);
    }
    store.lookup_dispatch_owner(nonce).ok().flatten().and_then(|bytes| Digest::from_slice(&bytes).map(BloomId))
}

pub(super) fn bloom_is_live(snapshot: &Snapshot, bloom: &BloomId) -> bool {
    snapshot.blooms.get(bloom).is_some_and(|record| is_active_unlanded(record.status))
}

/// The slug a registered worktree belongs to, when it is one of ours: exactly
/// `<base>/<slug>/tree`, both sides canonicalized.
pub(super) fn session_slug_of(base: &Path, worktree: &Path) -> Option<String> {
    if worktree.file_name()?.to_str()? != SESSION_TREE_DIR {
        return None;
    }
    child_name_of(base, worktree.parent()?)
}

/// The file name of `path` when its parent is exactly `base`, canonicalized on
/// both sides — the discriminator both checkout walks read their key with.
pub(super) fn child_name_of(base: &Path, path: &Path) -> Option<String> {
    let parent = fs::canonicalize(path.parent()?).ok()?;
    if parent != *base {
        return None;
    }
    Some(path.file_name()?.to_str()?.to_owned())
}

pub(super) fn evidence_nonce_of(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
    path.file_name()?.to_str()?.strip_suffix(EVIDENCE_SUFFIX).filter(|nonce| !nonce.is_empty()).map(str::to_owned)
}

pub(super) fn age_days(path: &Path, now: SystemTime) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified).ok().map(|elapsed| elapsed.as_secs() / SECS_PER_DAY)
}

pub(super) fn evidence_is_protected(
    live: &HashSet<&str>,
    snapshot: &Snapshot,
    nonce: &str,
    owner: Option<&BloomId>,
) -> bool {
    // An outstanding order is the narrowest durable evidence that a lane may
    // still be standing in this directory. The journal cannot speak for a
    // dispatch whose owner is not a sealed bloom — the pre-bloom scoping run
    // (ADR-0208) is exactly that shape — and whether that order is abandoned is
    // not this walk's question: the executor reconciles the outstanding set
    // against its own runs at boot, re-adopting the ones it recognizes and
    // reclaiming the rest, and faults an order whose lane is gone. The
    // directory becomes eligible when the order stops being outstanding.
    if live.contains(nonce) {
        return true;
    }
    // Consumed, but the bloom is still working — keep for forensics and
    // the calibration window until that bloom itself is terminal.
    owner.is_some_and(|bloom| bloom_is_live(snapshot, bloom))
}

/// Every session slug still entitled to its tree: one named by a member of a
/// bloom that is active and unlanded and not itself withdrawn from it, and one
/// named by an order that is still outstanding.
///
/// The second half is not a belt on the first. The journal is keyed by bloom
/// and only blooms are in it, so a dispatch whose order names no sealed bloom
/// is invisible to the membership walk however live it is — the pre-bloom
/// scoping run (ADR-0208) is exactly that shape: a real lane, working in a real
/// session tree, under a reserved digest nothing ever seals. Reading the
/// outstanding orders as well is what makes "no live work is bound to this
/// tree" a statement about the work rather than about the journal's coverage of
/// it, and an outstanding order is the narrowest durable evidence that a lane
/// may still be standing in the directory.
pub(super) fn live_session_slugs(
    store: &mut dyn StoreBackend,
    snapshot: &Snapshot,
    outstanding: &[String],
) -> rusqlite::Result<HashSet<String>> {
    let mut live = HashSet::new();
    for record in snapshot.blooms.values().filter(|record| is_active_unlanded(record.status)) {
        let bloom = record.spec.id();
        for member in record.spec.members() {
            if record.withdrawn.contains_key(&member.workpiece) {
                continue;
            }
            if let Some(slug) = store.lookup_session_slug(bloom.0.as_bytes(), &member.workpiece.0)? {
                live.insert(slug);
            }
        }
    }

    for nonce in outstanding {
        let Some(order) = store.lookup_order(nonce)? else {
            continue;
        };
        if let Some(slug) = store.lookup_session_slug(&order.bloom, &order.workpiece)? {
            live.insert(slug);
        }
    }
    Ok(live)
}

/// Retention window as a [`Duration`], for tests that age a directory.
#[cfg(test)]
pub(super) fn retention_duration(days: u64) -> Duration {
    Duration::from_secs(days.saturating_mul(SECS_PER_DAY))
}
