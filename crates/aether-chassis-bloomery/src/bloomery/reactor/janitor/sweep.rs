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

use aether_bloomery::{
    BloomId, BloomStatus, Digest, Event, ResolvedConfigs, Snapshot, SpendWindow, is_active_unlanded, reduce,
};
use aether_data::wire::from_bytes;

use crate::bloomery::SourceShell;
use crate::bloomery::TransformRunner;
use crate::store::StoreBackend;

/// The suffix a dispatch's evidence directory carries under the scratch root —
/// the same spelling [`LocalExecutor`](crate::bloomery::LocalExecutor) uses.
const EVIDENCE_SUFFIX: &str = "-evidence";

/// The prefix a lane slot's checkout or target directory carries.
const SLOT_PREFIX: &str = "slot-";

/// The suffix a lane slot's cargo target directory carries.
const TARGET_SUFFIX: &str = "-target";

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
    /// Nonce-keyed worktrees released because their owning run is terminal.
    pub worktrees: usize,
    /// Consumed evidence directories past the retention window.
    pub evidence_dirs: usize,
    /// Candidate / integration / checkpoint refs deleted for terminal blooms.
    pub refs: usize,
    /// Slot target directories removed because the host was over budget and idle.
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
    /// Whether a local lane child is in flight — blocks the target-dir sweep.
    pub lanes_running: bool,
    /// Retention and budget.
    pub policy: &'a JanitorPolicy,
    /// The clock retention is measured against. Production passes
    /// [`SystemTime::now`]; tests pin it so a suite is not a function of when
    /// it ran.
    pub now: SystemTime,
}

/// Rebuild the snapshot from the journal, then reclaim terminal worktrees,
/// retained-past-window evidence, terminal-bloom working refs, and over-budget
/// idle target dirs. Best-effort throughout: a dir git refuses or a ref the
/// source cannot delete is logged and stepped over, never a reason to skip the
/// rest of the pass.
///
/// # Errors
/// The store could not be read. A journal that does not decode is logged and
/// treated as empty rather than failed — a torn record must not take the
/// coordinator's janitor down.
pub fn sweep(request: &mut SweepRequest<'_>) -> rusqlite::Result<SweepReport> {
    let snapshot = replay_snapshot(request.store)?;
    let outstanding = request.store.list_outstanding_nonces()?;
    let live: HashSet<&str> = outstanding.iter().map(String::as_str).collect();
    let owners = dispatch_owners(request.store, &outstanding)?;

    let worktrees = reclaim_worktrees(request.runner, request.worktree_base, &live, &owners, &snapshot);
    let evidence_dirs = reclaim_evidence(request, &live, &owners, &snapshot);
    let refs = request.source.map_or(0, |source| prune_terminal_refs(source, &snapshot));
    let target_dirs = if request.lanes_running {
        0
    } else {
        sweep_targets_if_over_budget(request.target_base, request.policy.lane_target_budget_bytes)
    };
    Ok(SweepReport { worktrees, evidence_dirs, refs, target_dirs })
}

fn replay_snapshot(store: &mut dyn StoreBackend) -> rusqlite::Result<Snapshot> {
    let mut configs = ResolvedConfigs::default();
    for record in store.load_configs()? {
        let Some(address) = Digest::from_slice(&record.digest) else {
            continue;
        };
        configs.insert(address, record.kind, record.bytes);
    }

    let mut snapshot = Snapshot::default();
    for record in store.replay_journal()? {
        let Ok(event) = from_bytes::<Event>(&record.event) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                sequence = record.sequence,
                "janitor: journal record did not decode; skipping",
            );
            continue;
        };
        let decisions = reduce(&snapshot, &event, &configs, &SpendWindow::default());
        snapshot = snapshot.apply(&event, &decisions, &configs);
    }
    Ok(snapshot)
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
        reclaim_checkout(runner, &worktree);
        reclaimed += 1;
    }
    reclaimed
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
        remove_abandoned(&path);
        reclaimed += 1;
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

fn sweep_targets_if_over_budget(target_base: &Path, budget_bytes: u64) -> usize {
    let dirs = slot_target_dirs(target_base);
    if dirs.is_empty() {
        return 0;
    }
    let total: u64 = dirs.iter().map(|dir| dir_bytes(dir)).sum();
    if total <= budget_bytes {
        return 0;
    }
    tracing::info!(
        target: "aether_chassis_bloomery::janitor",
        total_bytes = total,
        budget_bytes,
        "janitor: lane target dirs over budget and idle; reclaiming",
    );
    let mut removed = 0;
    for dir in dirs {
        remove_abandoned(&dir);
        removed += 1;
    }
    removed
}

fn slot_target_dirs(target_base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(target_base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.file_name().and_then(|name| name.to_str()).is_some_and(is_slot_target))
        .collect()
}

fn is_slot_target(name: &str) -> bool {
    name.strip_prefix(SLOT_PREFIX)
        .and_then(|rest| rest.strip_suffix(TARGET_SUFFIX))
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_slot_directory(name: &str) -> bool {
    name.strip_prefix(SLOT_PREFIX)
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

fn scratch_nonce_of(base: &Path, worktree: &Path) -> Option<String> {
    let parent = fs::canonicalize(worktree.parent()?).ok()?;
    if parent != *base {
        return None;
    }
    Some(worktree.file_name()?.to_str()?.to_owned())
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

fn dir_bytes(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(child);
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

fn reclaim_checkout(runner: &dyn TransformRunner, dir: &Path) {
    if let Err(error) = runner.release(dir) {
        tracing::warn!(
            target: "aether_chassis_bloomery::janitor",
            worktree = %dir.display(),
            %error,
            "janitor: git refused an abandoned checkout; removing the directory directly",
        );
        remove_abandoned(dir);
    }
}

fn remove_abandoned(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            target: "aether_chassis_bloomery::janitor",
            path = %path.display(),
            %error,
            "janitor: abandoned directory could not be removed",
        ),
    }
}

fn hex_of(digest: &Digest) -> String {
    let bytes = digest.as_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(*byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(*byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Retention window as a [`Duration`], for tests that age a directory.
#[cfg(test)]
pub(super) fn retention_duration(days: u64) -> Duration {
    Duration::from_secs(days.saturating_mul(SECS_PER_DAY))
}
