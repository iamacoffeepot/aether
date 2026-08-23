//! The journal-driven sweep: rebuild the snapshot, then reclaim whatever the
//! journal says is terminal. Factored out of the reactor so tests drive it
//! against a `SqliteStore` and a stub [`TransformRunner`] without the mail
//! harness.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;
use std::time::SystemTime;

use aether_bloomery::{BloomId, BloomStatus, Digest, Snapshot, is_active_unlanded};

use crate::bloomery::LaneOccupancy;
use crate::bloomery::SourceShell;
use crate::bloomery::TransformRunner;
use crate::store::{StoreBackend, membership};

/// The suffix a dispatch's evidence directory carries under the scratch root —
/// the same spelling [`LocalExecutor`](crate::bloomery::LocalExecutor) uses.
const EVIDENCE_SUFFIX: &str = "-evidence";

/// The prefix a lane slot's fallback checkout or target directory carries.
const SLOT_PREFIX: &str = "slot-";

/// The directory under the scratch root holding one checkout per reusable
/// harness session, and the working tree inside each — the same spelling
/// [`LocalExecutor`](crate::bloomery::LocalExecutor) lays them out under.
const SESSIONS_DIR: &str = "sessions";
const SESSION_TREE_DIR: &str = "tree";

/// The suffix a lane slot's cargo target directory carries.
const TARGET_SUFFIX: &str = "-target";

/// The marker an evicted target directory wears between the rename that takes
/// it out of the build path and the removal that frees its bytes.
const EVICTING_SUFFIX: &str = ".evicting-";

/// Seconds in a day — the unit [`JanitorPolicy::evidence_retention_days`] is
/// stated in.
const SECS_PER_DAY: u64 = 86_400;

/// Configured retention and budget the sweep applies.
#[derive(Clone, Copy, Debug)]
pub struct JanitorPolicy {
    /// Combined size ceiling across every `slot-*-target` directory.
    pub lane_target_budget_bytes: u64,
    /// Days to keep consumed evidence of a terminal bloom.
    pub evidence_retention_days: u64,
}

/// What one sweep pass reclaimed, for the log line and for tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Checkouts released because the work they belong to is terminal: a
    /// nonce-keyed one whose owning run is over, and a session tree no live
    /// member's conversation is bound to any more.
    pub worktrees: usize,
    /// Consumed evidence directories past the retention window.
    pub evidence_dirs: usize,
    /// Candidate / integration / checkpoint refs deleted for terminal blooms.
    pub refs: usize,
    /// Slot target directories whose bytes this pass actually returned to the
    /// disk, evicting toward the budget or clearing what an earlier eviction
    /// left behind. Removals, never attempts: a directory the sweep tried and
    /// failed to remove is not one of these.
    pub target_dirs: usize,
}

/// The seams one sweep pass reads and writes.
pub struct SweepRequest<'a> {
    /// The journal the snapshot is rebuilt from, and the outstanding-order /
    /// dispatch-owner tables the disk walk is keyed by.
    pub store: &'a mut dyn StoreBackend,
    /// The spawn seam that lists registered worktrees and tears them down.
    pub runner: &'a dyn TransformRunner,
    /// The source that deletes working refs. `None` skips ref prune.
    pub source: Option<&'a SourceShell>,
    /// Scratch-worktree base: nonce-keyed checkouts and `*-evidence` dirs.
    pub worktree_base: &'a Path,
    /// Per-slot cargo target directory root.
    pub target_base: &'a Path,
    /// The live lane-occupancy probe, consulted immediately before each target
    /// directory is evicted rather than sampled once for the pass.
    ///
    /// A sampled `bool` is what let the 2026-08-14 incident through. A pass
    /// replays the whole journal, prunes a terminal bloom's refs over the
    /// network, and walks every target tree for its size before it reaches the
    /// eviction — seconds at least, and the reading it acted on came from
    /// before all of that. A slot frees and is claimed again inside that
    /// window, so what the sweep believed was an idle host was one with a
    /// compiler running in the directory it deleted.
    pub lanes: &'a dyn Fn() -> LaneOccupancy,
    /// Retention and budget.
    pub policy: &'a JanitorPolicy,
    /// The clock retention is measured against. Production passes
    /// [`SystemTime::now`]; tests pin it so a suite is not a function of when
    /// it ran.
    pub now: SystemTime,
}

/// Rebuild the snapshot from the journal, then reclaim terminal worktrees,
/// retained-past-window evidence, terminal-bloom working refs, and enough
/// over-budget target dirs to get back under the budget. Best-effort
/// throughout: a dir git refuses or a ref the source cannot delete is logged
/// and stepped over, never a reason to skip the rest of the pass.
///
/// The snapshot comes from the shared replay that folds each row's *recorded*
/// decisions (ADR-0190), not from re-deciding the journal with this binary's
/// reducer. Every reclaim here is a statement about the board the coordinator
/// is actually walking, and a re-decision reconstructs a different board: a
/// row whose recorded outcome this reducer no longer reproduces sends the
/// replay off the real history, and because the seal door admits one active
/// bloom per mainline, a landing lost that way refuses every seal after it.
/// The live bloom then does not exist as far as the sweep can see, and the
/// checkouts, evidence, and refs its members are still standing in read as
/// nobody's.
///
/// # Errors
/// The store could not be read. A journal row that does not decode is logged
/// and left out rather than failed — a torn record must not take the
/// coordinator's janitor down.
pub fn sweep(request: &mut SweepRequest<'_>) -> rusqlite::Result<SweepReport> {
    let snapshot = membership::replay_snapshot(request.store)?;
    let outstanding = request.store.list_outstanding_nonces()?;
    let live: HashSet<&str> = outstanding.iter().map(String::as_str).collect();
    let owners = dispatch_owners(request.store, &outstanding)?;

    let worktrees = reclaim_worktrees(request.runner, request.worktree_base, &live, &owners, &snapshot)
        + reclaim_session_trees(request.runner, request.worktree_base, request.store, &snapshot, &outstanding)?;
    let evidence_dirs = reclaim_evidence(request, &live, &owners, &snapshot);
    let refs = request.source.map_or(0, |source| prune_terminal_refs(source, &snapshot));
    let target_dirs = sweep_targets(request.target_base, request.policy.lane_target_budget_bytes, request.lanes);
    Ok(SweepReport { worktrees, evidence_dirs, refs, target_dirs })
}

/// Nonce → owning bloom, from outstanding rows and the durable owner table.
fn dispatch_owners(store: &mut dyn StoreBackend, outstanding: &[String]) -> rusqlite::Result<HashMap<String, BloomId>> {
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

fn bloom_of(store: &mut dyn StoreBackend, nonce: &str, owners: &HashMap<String, BloomId>) -> Option<BloomId> {
    if let Some(bloom) = owners.get(nonce) {
        return Some(*bloom);
    }
    store.lookup_dispatch_owner(nonce).ok().flatten().and_then(|bytes| Digest::from_slice(&bytes).map(BloomId))
}

fn bloom_is_live(snapshot: &Snapshot, bloom: &BloomId) -> bool {
    snapshot.blooms.get(bloom).is_some_and(|record| is_active_unlanded(record.status))
}

fn reclaim_worktrees(
    runner: &dyn TransformRunner,
    worktree_base: &Path,
    live: &HashSet<&str>,
    owners: &HashMap<String, BloomId>,
    snapshot: &Snapshot,
) -> usize {
    let Ok(base) = fs::canonicalize(worktree_base) else {
        return 0;
    };
    let registered = match runner.registered_worktrees() {
        Ok(registered) => registered,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                %error,
                "janitor: worktree registrations unreadable; abandoned checkouts not reclaimed",
            );
            return 0;
        }
    };

    let mut reclaimed = 0;
    for worktree in registered {
        let Some(nonce) = scratch_nonce_of(&base, &worktree) else {
            continue;
        };
        if is_slot_directory(&nonce) {
            continue;
        }
        if live.contains(nonce.as_str()) && owners.get(&nonce).is_some_and(|bloom| bloom_is_live(snapshot, bloom)) {
            continue;
        }
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            %nonce,
            worktree = %worktree.display(),
            "janitor: reclaiming a scratch checkout whose owning run is terminal",
        );
        if reclaim_checkout(runner, &worktree) {
            reclaimed += 1;
        }
    }
    reclaimed
}

/// Release the checkout of every harness session no live member is bound to
/// any more (#5425).
///
/// A session's tree is created when the session is minted and reused by every
/// launch of that conversation — a member's own retry laps, and a declared-edge
/// dependent that inherits it — so nothing inside the session's life may remove
/// it. What ends it is the work ending: no order under this slug is outstanding
/// any more, and every member whose row names it landed with its bloom, was
/// withdrawn from it, or left with it when the bloom was superseded or
/// cancelled. Everything the live set does not name is reclaimable, which also
/// clears the trees of a bloom this process never saw walk.
///
/// The candidates are the repository's *registered* worktrees, filtered to the
/// `sessions/<slug>/tree` shape, for the reason [`reclaim_worktrees`] filters to
/// children of the base: the scratch root is a configured path, and a
/// registration is the only positive proof a directory under it is one of ours.
///
/// The session's compiled artifacts inside each slot target are deliberately
/// not chased. Cargo names its fingerprint and incremental directories by a
/// metadata hash this process cannot derive from a source path, and a
/// hand-removal of part of a target directory is not an operation cargo
/// supports — a half-removed fingerprint set fails the next build in ways that
/// read as a regression in the candidate. What bounds that growth is the budget
/// eviction below, which takes a whole target out atomically.
///
/// # Errors
/// The store could not be read for the live members' slugs.
fn reclaim_session_trees(
    runner: &dyn TransformRunner,
    worktree_base: &Path,
    store: &mut dyn StoreBackend,
    snapshot: &Snapshot,
    outstanding: &[String],
) -> rusqlite::Result<usize> {
    let Ok(base) = fs::canonicalize(worktree_base.join(SESSIONS_DIR)) else {
        return Ok(0);
    };
    let registered = match runner.registered_worktrees() {
        Ok(registered) => registered,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                %error,
                "janitor: worktree registrations unreadable; terminal session trees not reclaimed",
            );
            return Ok(0);
        }
    };
    let live = live_session_slugs(store, snapshot, outstanding)?;

    let mut reclaimed = 0;
    for worktree in registered {
        let Some(slug) = session_slug_of(&base, &worktree) else {
            continue;
        };
        if live.contains(slug.as_str()) {
            continue;
        }
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            %slug,
            worktree = %worktree.display(),
            "janitor: reclaiming the checkout of a session no live member is bound to",
        );
        if reclaim_checkout(runner, &worktree) {
            reclaimed += 1;
        }
    }
    Ok(reclaimed)
}

/// The slug a registered worktree belongs to, when it is one of ours: exactly
/// `<base>/<slug>/tree`, both sides canonicalized.
fn session_slug_of(base: &Path, worktree: &Path) -> Option<String> {
    if worktree.file_name()?.to_str()? != SESSION_TREE_DIR {
        return None;
    }
    child_name_of(base, worktree.parent()?)
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
fn live_session_slugs(
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

fn reclaim_evidence(
    request: &mut SweepRequest<'_>,
    live: &HashSet<&str>,
    owners: &HashMap<String, BloomId>,
    snapshot: &Snapshot,
) -> usize {
    let entries = match fs::read_dir(request.worktree_base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return 0,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                %error,
                "janitor: evidence root unreadable",
            );
            return 0;
        }
    };

    let mut reclaimed = 0;
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
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            %nonce,
            evidence = %path.display(),
            "janitor: reclaiming a consumed evidence directory past the retention window",
        );
        if remove_abandoned(&path) {
            reclaimed += 1;
        }
    }
    reclaimed
}

fn evidence_is_protected(live: &HashSet<&str>, snapshot: &Snapshot, nonce: &str, owner: Option<&BloomId>) -> bool {
    // Intake has not consumed this directory. A live bloom still needs it;
    // a terminal bloom's abandoned leftover is eligible once the window
    // passes (a kill/crash never reached intake).
    if live.contains(nonce) {
        return owner.is_some_and(|bloom| bloom_is_live(snapshot, bloom));
    }
    // Consumed, but the bloom is still working — keep for forensics and
    // the calibration window until that bloom itself is terminal.
    owner.is_some_and(|bloom| bloom_is_live(snapshot, bloom))
}

fn prune_terminal_refs(source: &SourceShell, snapshot: &Snapshot) -> usize {
    let mut pruned = 0;
    for (bloom, record) in &snapshot.blooms {
        if !matches!(record.status, BloomStatus::Landed | BloomStatus::Superseded) {
            continue;
        }
        match source.prune_working_refs(bloom) {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(
                        target: "aether_chassis_bloomery::janitor",
                        bloom = %format_args!("{}", hex_of(&bloom.0)),
                        pruned = count,
                        "janitor: pruned working refs of a terminal bloom",
                    );
                }
                pruned += count;
            }
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                %error,
                "janitor: working-ref prune failed; will retry next tick",
            ),
        }
    }
    pruned
}

/// One slot target directory as the eviction ranks it.
struct SlotTarget {
    /// The slot index the directory belongs to — what the occupancy reading is
    /// keyed by.
    slot: usize,
    path: PathBuf,
    bytes: u64,
    /// Newest mtime anywhere in the tree: the recency the eviction orders by.
    used: SystemTime,
}

/// Bring the slot target directories back under budget, evicting the coldest
/// first and only as far as the budget requires.
///
/// Reclaiming *everything* on an overage — what this did until the 2026-08-14
/// incident — makes each firing maximally destructive, and a budget set below
/// the working set makes it fire forever: the host can never get under, so
/// every tick wants to sweep and races every lane start. Evicting to the line
/// keeps the warm caches that are still inside it.
fn sweep_targets(target_base: &Path, budget_bytes: u64, lanes: &dyn Fn() -> LaneOccupancy) -> usize {
    let mut removed = reclaim_evicting_leftovers(target_base);
    let mut targets = slot_targets(target_base);
    let total: u64 = targets.iter().map(|target| target.bytes).sum();
    if total <= budget_bytes {
        return removed;
    }

    let occupancy = lanes();
    tracing::info!(
        target: "aether_chassis_bloomery::janitor",
        total_bytes = total,
        budget_bytes,
        lanes_running = occupancy.any_running(),
        occupied_slots = occupancy.slots.len(),
        "janitor: lane target dirs over budget; evicting least recently used back to the line",
    );

    // Coldest first: the slot nobody has built in for longest is the one whose
    // loss costs the least rebuild time.
    targets.sort_by_key(|target| target.used);
    let mut held = total;
    for target in targets {
        if held <= budget_bytes {
            break;
        }
        // Live, per directory. Everything above this point — the size walk
        // especially — takes long enough for the answer to have changed.
        let occupancy = lanes();
        if occupancy.unattributed {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                "janitor: a lane child is running in an unidentified slot; leaving every target dir in place",
            );
            break;
        }
        if occupancy.slots.contains(&target.slot) {
            tracing::info!(
                target: "aether_chassis_bloomery::janitor",
                slot = target.slot,
                "janitor: slot is occupied; its target dir stays even though the host is over budget",
            );
            continue;
        }
        if evict_target_dir(&target.path) {
            held = held.saturating_sub(target.bytes);
            removed += 1;
        }
    }
    removed
}

fn slot_targets(target_base: &Path) -> Vec<SlotTarget> {
    let Ok(entries) = fs::read_dir(target_base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let slot = slot_target_index(path.file_name()?.to_str()?)?;
            if !path.is_dir() {
                return None;
            }
            let usage = dir_usage(&path);
            Some(SlotTarget { slot, path, bytes: usage.bytes, used: usage.newest })
        })
        .collect()
}

fn slot_target_index(name: &str) -> Option<usize> {
    let index = name.strip_prefix(SLOT_PREFIX)?.strip_suffix(TARGET_SUFFIX)?;
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}

fn is_slot_directory(name: &str) -> bool {
    name.strip_prefix(SLOT_PREFIX)
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

fn scratch_nonce_of(base: &Path, worktree: &Path) -> Option<String> {
    child_name_of(base, worktree)
}

/// The file name of `path` when its parent is exactly `base`, canonicalized on
/// both sides — the discriminator both checkout sweeps read their key with.
fn child_name_of(base: &Path, path: &Path) -> Option<String> {
    let parent = fs::canonicalize(path.parent()?).ok()?;
    if parent != *base {
        return None;
    }
    Some(path.file_name()?.to_str()?.to_owned())
}

fn evidence_nonce_of(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
    path.file_name()?.to_str()?.strip_suffix(EVIDENCE_SUFFIX).filter(|nonce| !nonce.is_empty()).map(str::to_owned)
}

fn age_days(path: &Path, now: SystemTime) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified).ok().map(|elapsed| elapsed.as_secs() / SECS_PER_DAY)
}

/// What one directory tree costs and when it was last written.
struct DirUsage {
    bytes: u64,
    newest: SystemTime,
}

/// Total bytes and newest mtime under `path`, in one walk.
///
/// Recency comes out of the same walk the size does because the walk is the
/// expensive part — a slot target tree is tens of gigabytes across millions of
/// files, and statting it twice to learn two facts about it would double the
/// window the eviction is racing.
fn dir_usage(path: &Path) -> DirUsage {
    let mut bytes: u64 = 0;
    let mut newest = fs::metadata(path).and_then(|metadata| metadata.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut stack = vec![path.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if let Ok(modified) = metadata.modified() {
                newest = newest.max(modified);
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    DirUsage { bytes, newest }
}

/// Take a target directory out of the build path atomically, then delete it.
///
/// The rename is the safety, not a tidiness. `remove_dir_all` walks the tree
/// unlinking as it goes, so a compile racing the sweep is hollowed out
/// underneath itself — it watches its own object files disappear one at a time
/// and fails on whichever one it happened to reach for, which is exactly the
/// 2026-08-14 incident's `unable to copy … No such file or directory`. A
/// rename is one atomic step: the racing compile finds the whole directory
/// gone at once and fails immediately with a diagnosable error, or never
/// notices because it had not opened anything under it yet.
///
/// Reports whether the bytes are actually gone, so the caller counts removals
/// rather than attempts.
fn evict_target_dir(path: &Path) -> bool {
    let Some(aside) = eviction_path(path) else {
        tracing::warn!(
            target: "aether_chassis_bloomery::janitor",
            path = %path.display(),
            "janitor: target dir has no nameable sibling to move aside into; left in place",
        );
        return false;
    };
    if let Err(error) = fs::rename(path, &aside) {
        // Deliberately not falling back to an in-place removal: deleting where
        // it stands is the hollowing-out this function exists to avoid, and the
        // next pass retries.
        tracing::warn!(
            target: "aether_chassis_bloomery::janitor",
            path = %path.display(),
            %error,
            "janitor: target dir could not be moved aside; left in place rather than deleted underneath a compile",
        );
        return false;
    }
    remove_abandoned(&aside)
}

fn eviction_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_nanos());
    Some(path.parent()?.join(format!("{name}{EVICTING_SUFFIX}{stamp}")))
}

/// Delete the moved-aside directories a previous pass renamed but could not
/// remove, returning how many went.
///
/// Unswept, they are permanent and invisible: the rename takes a tree out of
/// the `slot-<index>-target` namespace the budget is measured over, so a failed
/// removal would hide tens of gigabytes from the accounting while they sit on
/// the disk. Runs before the budget test rather than inside it, because a host
/// that has dropped back under budget is exactly the one that would otherwise
/// keep them forever.
fn reclaim_evicting_leftovers(target_base: &Path) -> usize {
    let Ok(entries) = fs::read_dir(target_base) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_evicting_leftover(&path) || !path.is_dir() {
            continue;
        }
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            path = %path.display(),
            "janitor: retrying a target dir a previous pass moved aside but could not remove",
        );
        if remove_abandoned(&path) {
            removed += 1;
        }
    }
    removed
}

/// Whether a name is one this janitor minted in [`eviction_path`] — a slot
/// target name, the marker, and the stamp, all three. Strict, so the retry can
/// only ever act on its own leavings.
fn is_evicting_leftover(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).and_then(|name| name.split_once(EVICTING_SUFFIX)).is_some_and(
        |(target, stamp)| {
            slot_target_index(target).is_some() && !stamp.is_empty() && stamp.bytes().all(|byte| byte.is_ascii_digit())
        },
    )
}

/// Release one abandoned checkout, reporting whether it is actually gone.
fn reclaim_checkout(runner: &dyn TransformRunner, dir: &Path) -> bool {
    let Err(error) = runner.release(dir) else {
        return true;
    };
    tracing::warn!(
        target: "aether_chassis_bloomery::janitor",
        worktree = %dir.display(),
        %error,
        "janitor: git refused an abandoned checkout; removing the directory directly",
    );
    remove_abandoned(dir)
}

/// Remove a directory tree, reporting whether it is gone.
///
/// The return value is load-bearing: a caller that counts calls rather than
/// successes reports a reclamation that did not happen, which is how the
/// 2026-08-14 incident logged `target_dirs=2` in the same breath as
/// `Directory not empty`.
fn remove_abandoned(path: &Path) -> bool {
    match fs::remove_dir_all(path) {
        Ok(()) => true,
        // Already absent is the state this asked for.
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                path = %path.display(),
                %error,
                "janitor: abandoned directory could not be removed",
            );
            false
        }
    }
}

fn hex_of(digest: &Digest) -> String {
    digest.to_hex()
}

/// Retention window as a [`Duration`], for tests that age a directory.
#[cfg(test)]
pub(super) fn retention_duration(days: u64) -> Duration {
    Duration::from_secs(days.saturating_mul(SECS_PER_DAY))
}
