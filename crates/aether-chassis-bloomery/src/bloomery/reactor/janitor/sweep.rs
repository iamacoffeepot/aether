//! The journal-driven sweep of **working state** (ADR-0211): nonce-keyed
//! dispatch checkouts, terminal-bloom working refs, and the moved-aside
//! leavings of past evictions. Factored out of the reactor so tests drive it
//! against a `SqliteStore` and a stub [`TransformRunner`] without the mail
//! harness.
//!
//! Records — evidence directories and resolved session trees — belong to the
//! archive pass, not this tick. Caches — cargo target directories — are the
//! only disk-pressure kill, and still run every tick.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::precheck::namespace as preview_namespace;
use aether_bloomery::{
    BloomId, BloomStatus, CandidatePreparationPayload, ConstructionAdmissionPayload, Digest, IntegrationAppendPayload,
    Nonce, PartialHeadRepairDispatch, PartialHeadRepairPayload, QueuePrecheckPlanPayload, SharedRunDispatch, Snapshot,
    Topic, construction_nonce_digest, is_active_unlanded,
};
use aether_bloomery_git::{construction_checkpoint_scope_namespace, partial_head_repair_namespace};
use aether_bloomery_github::SourceError;
use aether_data::wire::from_bytes;

use crate::bloomery::LaneOccupancy;
use crate::bloomery::SourceShell;
use crate::bloomery::TransformRunner;
use crate::bloomery::config::JANITOR_REF_PRUNES_PER_TICK;
use crate::bloomery::coordination::{
    generation_namespace, preparation_namespace, shared_probe_namespace, shared_run_namespace,
};
use crate::bloomery::reactor::shared_run::SharedStepDescriptor;
use crate::store::{SharedRunLifecycle, SharedRunRow, StoreBackend, membership};

use super::records::{between_blooms, bloom_is_live, child_name_of, dispatch_owners};

/// The prefix a lane slot's fallback checkout or target directory carries.
const SLOT_PREFIX: &str = "slot-";

/// The suffix a lane slot's cargo target directory carries.
const TARGET_SUFFIX: &str = "-target";

/// The marker an evicted target directory wears between the rename that takes
/// it out of the build path and the removal that frees its bytes.
const EVICTING_SUFFIX: &str = ".evicting-";

/// Configured retention and budget the sweep applies.
#[derive(Clone, Copy, Debug)]
pub struct JanitorPolicy {
    /// Combined size ceiling across every `slot-*-target` directory.
    pub lane_target_budget_bytes: u64,
    /// Floor between size walks, in seconds. Stated apart from the executor's
    /// poll interval because the two answer different questions: the poll is
    /// how often the pass runs at all, this is how often a pass that *could*
    /// evict is allowed to pay for `dir_usage`. `0` measures on every tick
    /// that has a free slot.
    pub target_scan_interval_secs: u64,
    /// Days a consumed evidence directory of a terminal bloom must age before
    /// an archive pass will move it. The tick no longer deletes evidence; the
    /// field stays on the policy so the pass and the sweep share one struct.
    pub evidence_retention_days: u64,
}

/// What one pass leaves the next: the free-slot set the last measurement was
/// taken against, and when.
///
/// The memo is neither a seam nor per-pass — it is process-local state the
/// reactor holds so an over-budget tick that cannot evict does not re-walk
/// the trees. Rebuilt empty on restart, which costs exactly one measurement.
#[derive(Clone, Debug, Default)]
pub struct TargetScan {
    measured_at: Option<SystemTime>,
    free_slots: BTreeSet<usize>,
}

impl TargetScan {
    /// True when `free` differs from the memo's set, when nothing has been
    /// measured yet, or when the interval has elapsed.
    fn should_measure(&self, free: &BTreeSet<usize>, interval: Duration, now: SystemTime) -> bool {
        let Some(measured_at) = self.measured_at else {
            return true;
        };
        if *free != self.free_slots {
            return true;
        }
        now.duration_since(measured_at).ok().is_none_or(|elapsed| elapsed >= interval)
    }

    fn record(&mut self, free: BTreeSet<usize>, now: SystemTime) {
        self.free_slots = free;
        self.measured_at = Some(now);
    }
}

/// What one sweep pass reclaimed, for the log line and for tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Nonce-keyed checkouts released because the work they belong to is
    /// terminal — an owning run that is over. Session trees are records and
    /// are not counted here.
    pub worktrees: usize,
    /// Candidate / integration / checkpoint refs deleted for terminal blooms.
    pub refs: usize,
    /// Slot target directories whose bytes this pass actually returned to the
    /// disk, evicting toward the budget or clearing what an earlier eviction
    /// left behind. Removals, never attempts: a directory the sweep tried and
    /// failed to remove is not one of these.
    pub target_dirs: usize,
    /// Slot target trees this pass walked for size. Separate from
    /// [`Self::target_dirs`]: a pass that measured nothing and a pass that
    /// measured everything both remove zero directories when the host is over
    /// budget with no free slot.
    pub targets_measured: usize,
}

/// The prune seam one janitor tick talks to. [`SourceShell`] is the production
/// implementor; tests stub it to count and fail calls.
pub trait WorkingRefPruner {
    /// Delete `bloom`'s candidate, integration, and checkpoint refs.
    ///
    /// # Errors
    /// A transport or backend fault other than an already-absent ref.
    fn prune_working_refs(&self, bloom: &BloomId) -> Result<usize, SourceError>;
}

impl WorkingRefPruner for SourceShell {
    fn prune_working_refs(&self, bloom: &BloomId) -> Result<usize, SourceError> {
        Self::prune_working_refs(self, bloom)
    }
}

/// The seams one sweep pass reads and writes.
pub struct SweepRequest<'a> {
    /// The journal the snapshot is rebuilt from, and the outstanding-order /
    /// dispatch-owner tables the disk walk is keyed by.
    pub store: &'a mut dyn StoreBackend,
    /// The spawn seam that lists registered worktrees and tears them down.
    pub runner: &'a dyn TransformRunner,
    /// The source that deletes working refs. `None` skips ref prune.
    pub source: Option<&'a dyn WorkingRefPruner>,
    /// Scratch-worktree base: nonce-keyed checkouts and `*-evidence` dirs.
    pub worktree_base: &'a Path,
    /// Per-slot cargo target directory root.
    pub target_base: &'a Path,
    /// The live lane-occupancy probe, consulted immediately before each target
    /// directory is evicted rather than sampled once for the pass.
    ///
    /// A sampled `bool` is what let the 2026-08-14 incident through. A pass
    /// replays the whole journal, prunes a terminal bloom's refs over the
    /// network, and may walk target trees for size before it reaches the
    /// eviction — seconds at least, and the reading it acted on came from
    /// before all of that. A slot frees and is claimed again inside that
    /// window, so what the sweep believed was an idle host was one with a
    /// compiler running in the directory it deleted.
    pub lanes: &'a dyn Fn() -> LaneOccupancy,
    /// Retention and budget.
    pub policy: &'a JanitorPolicy,
    /// The clock retention and target-tree measurement freshness are measured
    /// against. Production passes [`SystemTime::now`]; tests pin it so a suite
    /// is not a function of when it ran.
    pub now: SystemTime,
    /// Blooms this process has already pruned working refs for. A successful
    /// prune is recorded so a later tick does not pay for it again; a fault is
    /// left out so the next tick retries, matching the warn the prune already
    /// logs.
    pub pruned: &'a mut HashSet<BloomId>,
}

/// Rebuild the snapshot from the journal, then reclaim terminal nonce-keyed
/// worktrees, terminal-bloom working refs, and enough over-budget target dirs
/// to get back under the budget. Best-effort throughout: a dir git refuses or
/// a ref the source cannot delete is logged and stepped over, never a reason
/// to skip the rest of the pass.
///
/// The snapshot comes from the shared replay that folds each row's *recorded*
/// decisions (ADR-0190), not from re-deciding the journal with this binary's
/// reducer. Every reclaim here is a statement about the board the coordinator
/// is actually walking, and a re-decision reconstructs a different board: a
/// row whose recorded outcome this reducer no longer reproduces sends the
/// replay off the real history, and because the seal door admits one active
/// bloom per mainline, a landing lost that way refuses every seal after it.
/// The live bloom then does not exist as far as the sweep can see, and the
/// checkouts and refs its members are still standing in read as nobody's.
///
/// Nonce-keyed checkout reclaim runs only between blooms: no bloom in that
/// replayed snapshot is active-and-unlanded, and no order is outstanding.
/// Computed in this pass from those two sets, never sampled elsewhere or
/// cached across ticks. While anything walks, that sweep skips quietly.
/// Evidence directories and session trees are records (ADR-0211) and are not
/// deleted here — the operator archive pass moves them. Disk pressure is the
/// slot-target budget eviction, which still runs every tick and evicts only
/// those directories — regenerable build state, never source trees or text.
/// The target sweep measures on its own cadence: a size walk runs only when a
/// slot is free to evict and either occupancy has changed or the scan
/// interval has elapsed.
///
/// # Errors
/// The store could not be read. A journal row that does not decode is logged
/// and left out rather than failed — a torn record must not take the
/// coordinator's janitor down.
pub fn sweep(request: &mut SweepRequest<'_>, scan: &mut TargetScan) -> rusqlite::Result<SweepReport> {
    let snapshot = membership::replay_snapshot(request.store)?;
    let outstanding = request.store.list_outstanding_nonces()?;
    let live: HashSet<&str> = outstanding.iter().map(String::as_str).collect();
    let owners = dispatch_owners(request.store, &outstanding)?;
    let between_blooms = between_blooms(&snapshot, &outstanding);

    let worktrees = if between_blooms {
        reclaim_worktrees(request.runner, request.worktree_base, &live, &owners, &snapshot)
    } else {
        0
    };
    let mut refs = request.source.map_or(0, |source| prune_terminal_refs(source, &snapshot, request.pruned));
    if let Some(source) = request.source {
        refs += prune_preview_refs(request.store, source, &snapshot, request.pruned)?;
        refs += prune_coordination_refs(request.store, source, &snapshot, request.pruned)?;
    }
    let interval = Duration::from_secs(request.policy.target_scan_interval_secs);
    let now = request.now;
    let (target_dirs, targets_measured) = sweep_targets(request, scan, interval, now);
    Ok(SweepReport { worktrees, refs, target_dirs, targets_measured })
}

/// Reclaim only namespaces named by retained immutable plans. Current
/// generations and every context still retained by the reducer stay live;
/// shared-run and probe namespaces stay until their durable run is terminal.
/// One successful or attempted source prune bounds each class per tick.
fn prune_coordination_refs(
    store: &mut dyn StoreBackend,
    source: &dyn WorkingRefPruner,
    snapshot: &Snapshot,
    already: &mut HashSet<BloomId>,
) -> rusqlite::Result<usize> {
    let shared_runs = store.list_shared_runs()?;
    let retained_blooms = retained_coordination_blooms(store, snapshot, &shared_runs)?;
    let protected_generations = protected_coordination_generations(snapshot, &retained_blooms);
    let protected_repairs = protected_partial_head_repairs(store, snapshot)?;
    Ok(prune_preparation_refs(store, source, snapshot, &retained_blooms, already)?
        + prune_checkpoint_refs(store, source, snapshot, &retained_blooms, already)?
        + prune_generation_refs(store, source, &protected_generations, already)?
        + prune_partial_head_repair_refs(store, source, &protected_repairs, already)?
        + prune_shared_refs(store, source, shared_runs, already)?)
}

fn retained_coordination_blooms(
    store: &mut dyn StoreBackend,
    snapshot: &Snapshot,
    shared_runs: &[SharedRunRow],
) -> rusqlite::Result<HashSet<BloomId>> {
    let mut retained_blooms = HashSet::new();
    for nonce in store.list_order_nonces()? {
        if let Some(order) = store.lookup_order(&nonce)?
            && let Some(bloom) = Digest::from_slice(&order.bloom)
        {
            retained_blooms.insert(BloomId(bloom));
        }
    }
    for row in &shared_runs {
        if !shared_run_refs_are_live(store, row)? {
            continue;
        }
        let dispatch = from_bytes::<SharedRunDispatch>(&row.dispatch)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("shared-run retain row: {error}")))?;
        retained_blooms.extend(dispatch.plan.requests.iter().map(|request| request.bloom));
    }
    for (bloom, record) in &snapshot.blooms {
        if coordination_refs_are_live(record.status, retained_blooms.contains(bloom)) {
            retained_blooms.insert(*bloom);
        }
    }
    Ok(retained_blooms)
}

fn protected_coordination_generations(snapshot: &Snapshot, retained_blooms: &HashSet<BloomId>) -> HashSet<Digest> {
    let mut protected_generations = HashSet::new();
    for (bloom, record) in &snapshot.blooms {
        if !retained_blooms.contains(bloom) {
            continue;
        }
        let Some(state) = record.coordination.as_deref() else {
            continue;
        };
        protected_generations.insert(state.integration.generation.digest());
        protected_generations.extend(state.contexts.values().map(|context| context.starting_head.generation));
        protected_generations
            .extend(state.prepared.values().map(|candidate| candidate.context.starting_head.generation));
    }
    protected_generations
}

fn protected_partial_head_repairs(
    store: &mut dyn StoreBackend,
    snapshot: &Snapshot,
) -> rusqlite::Result<HashSet<Digest>> {
    let mut protected = snapshot
        .blooms
        .values()
        .filter_map(|record| record.coordination.as_deref())
        .flat_map(|state| {
            state
                .partial_head_repair
                .iter()
                .map(|plan| plan.digest())
                .chain((state.integration.head.plan != Digest::default()).then_some(state.integration.head.plan))
        })
        .collect::<HashSet<_>>();
    for row in store.list_partial_head_repairs()? {
        let dispatch = from_bytes::<PartialHeadRepairDispatch>(&row.dispatch)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head retain row: {error}")))?;
        protected.insert(dispatch.plan.digest());
    }
    Ok(protected)
}

fn prune_preparation_refs(
    store: &mut dyn StoreBackend,
    source: &dyn WorkingRefPruner,
    snapshot: &Snapshot,
    retained_blooms: &HashSet<BloomId>,
    already: &mut HashSet<BloomId>,
) -> rusqlite::Result<usize> {
    let mut pruned = 0;
    let mut attempted = 0;
    for entry in store.delivered_topic(Topic::CandidatePreparation)? {
        let payload = from_bytes::<CandidatePreparationPayload>(&entry.payload).map_err(|error| {
            rusqlite::Error::InvalidParameterName(format!("candidate preparation prune payload: {error}"))
        })?;
        let Some(record) = snapshot.blooms.get(&payload.plan.bloom) else {
            continue;
        };
        let namespace = preparation_namespace(&payload.plan);
        if retained_blooms.contains(&payload.plan.bloom)
            || !refs_are_reclaimable(snapshot, &payload.plan.bloom, record.status)
            || already.contains(&namespace)
        {
            continue;
        }
        if attempted >= JANITOR_REF_PRUNES_PER_TICK {
            break;
        }
        attempted += 1;
        pruned += prune_private_namespace(source, namespace, already, "terminal candidate preparation");
    }
    Ok(pruned)
}

fn prune_checkpoint_refs(
    store: &mut dyn StoreBackend,
    source: &dyn WorkingRefPruner,
    snapshot: &Snapshot,
    retained_blooms: &HashSet<BloomId>,
    already: &mut HashSet<BloomId>,
) -> rusqlite::Result<usize> {
    let mut pruned = 0;
    let mut attempted = 0;
    for entry in store.delivered_topic(Topic::ConstructionAdmission)? {
        let payload = from_bytes::<ConstructionAdmissionPayload>(&entry.payload)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("checkpoint prune payload: {error}")))?;
        let dispatch = payload.dispatch;
        let Some(nonce) = store.construction_admission_nonce(dispatch.digest().as_bytes())? else {
            continue;
        };
        let nonce = construction_nonce_digest(&Nonce(nonce));
        let namespace = construction_checkpoint_scope_namespace(
            dispatch.bloom,
            &dispatch.workpiece,
            dispatch.scope_revision,
            dispatch.context.starting_head.candidate.checkout,
            nonce,
        );
        let retained = retained_blooms.contains(&dispatch.bloom)
            && snapshot.blooms.get(&dispatch.bloom).and_then(|record| record.coordination.as_deref()).is_some_and(
                |state| {
                    state
                        .admitted_construction
                        .get(&dispatch.workpiece)
                        .is_some_and(|admission| admission.nonce == nonce && admission.dispatch == dispatch)
                        || state.checkpoints.get(&dispatch.workpiece).is_some_and(|checkpoint| {
                            checkpoint.nonce == nonce
                                && checkpoint.scope_revision == dispatch.scope_revision
                                && checkpoint.starting_checkout == dispatch.context.starting_head.candidate.checkout
                        })
                },
            );
        if retained || already.contains(&namespace) {
            continue;
        }
        if attempted >= JANITOR_REF_PRUNES_PER_TICK {
            break;
        }
        attempted += 1;
        pruned += prune_private_namespace(source, namespace, already, "retired construction checkpoints");
    }
    Ok(pruned)
}

fn prune_partial_head_repair_refs(
    store: &mut dyn StoreBackend,
    source: &dyn WorkingRefPruner,
    protected: &HashSet<Digest>,
    already: &mut HashSet<BloomId>,
) -> rusqlite::Result<usize> {
    let mut pruned = 0;
    let mut attempted = 0;
    for entry in store.delivered_topic(Topic::PartialHeadRepair)? {
        let payload = from_bytes::<PartialHeadRepairPayload>(&entry.payload)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head prune payload: {error}")))?;
        let plan = payload.dispatch.plan.digest();
        let namespace = partial_head_repair_namespace(plan);
        if protected.contains(&plan) || already.contains(&namespace) {
            continue;
        }
        if attempted >= JANITOR_REF_PRUNES_PER_TICK {
            break;
        }
        attempted += 1;
        pruned += prune_private_namespace(source, namespace, already, "retired partial-head repair");
    }
    Ok(pruned)
}

fn prune_generation_refs(
    store: &mut dyn StoreBackend,
    source: &dyn WorkingRefPruner,
    protected_generations: &HashSet<Digest>,
    already: &mut HashSet<BloomId>,
) -> rusqlite::Result<usize> {
    let mut pruned = 0;
    let mut attempted = 0;
    for entry in store.delivered_topic(Topic::IntegrationAppend)? {
        let payload = from_bytes::<IntegrationAppendPayload>(&entry.payload)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("eager append prune payload: {error}")))?;
        let namespace = generation_namespace(payload.plan.generation);
        if protected_generations.contains(&payload.plan.generation) || already.contains(&namespace) {
            continue;
        }
        if attempted >= JANITOR_REF_PRUNES_PER_TICK {
            break;
        }
        attempted += 1;
        pruned += prune_private_namespace(source, namespace, already, "obsolete eager generation");
    }
    Ok(pruned)
}

fn prune_shared_refs(
    store: &mut dyn StoreBackend,
    source: &dyn WorkingRefPruner,
    shared_runs: Vec<SharedRunRow>,
    already: &mut HashSet<BloomId>,
) -> rusqlite::Result<usize> {
    let mut pruned = 0;
    let mut attempted = 0;
    for row in shared_runs {
        if shared_run_refs_are_live(store, &row)? {
            continue;
        }
        let dispatch = from_bytes::<SharedRunDispatch>(&row.dispatch)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("shared-run prune row: {error}")))?;
        if attempted >= JANITOR_REF_PRUNES_PER_TICK {
            break;
        }
        let namespace = shared_run_namespace(&dispatch.plan);
        if dispatch.plan.composition.is_some() && !already.contains(&namespace) {
            attempted += 1;
            pruned += prune_private_namespace(source, namespace, already, "terminal shared run");
            if !already.contains(&namespace) {
                continue;
            }
        }
        for step in store.shared_run_steps(&row.run)? {
            let Ok(SharedStepDescriptor::Probe(probe)) = serde_json::from_slice(&step.descriptor) else {
                continue;
            };
            let namespace = shared_probe_namespace(probe.run, probe.ordinal, probe.plan);
            if already.contains(&namespace) || attempted >= JANITOR_REF_PRUNES_PER_TICK {
                continue;
            }
            attempted += 1;
            pruned += prune_private_namespace(source, namespace, already, "terminal shared probe");
        }
    }
    Ok(pruned)
}

fn coordination_refs_are_live(status: BloomStatus, has_durable_work: bool) -> bool {
    is_active_unlanded(status) || has_durable_work
}

fn shared_run_refs_are_live(store: &mut dyn StoreBackend, row: &SharedRunRow) -> rusqlite::Result<bool> {
    if matches!(
        row.lifecycle,
        SharedRunLifecycle::Preparing
            | SharedRunLifecycle::Ready
            | SharedRunLifecycle::Running
            | SharedRunLifecycle::Completing
    ) {
        return Ok(true);
    }
    for step in store.shared_run_steps(&row.run)? {
        if step.receipt.is_none() || store.lookup_order(&step.nonce)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn prune_private_namespace(
    source: &dyn WorkingRefPruner,
    namespace: BloomId,
    already: &mut HashSet<BloomId>,
    class: &str,
) -> usize {
    match source.prune_working_refs(&namespace) {
        Ok(count) => {
            already.insert(namespace);
            count
        }
        Err(error) => {
            tracing::warn!(%error, %class, "janitor: private namespace prune failed; will retry");
            0
        }
    }
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

fn prune_terminal_refs(source: &dyn WorkingRefPruner, snapshot: &Snapshot, already: &mut HashSet<BloomId>) -> usize {
    let mut pruned = 0;
    let mut attempted = 0;
    for (bloom, record) in &snapshot.blooms {
        if !refs_are_reclaimable(snapshot, bloom, record.status) {
            continue;
        }
        if already.contains(bloom) {
            continue;
        }
        if attempted >= JANITOR_REF_PRUNES_PER_TICK {
            break;
        }
        attempted += 1;
        match source.prune_working_refs(bloom) {
            Ok(count) => {
                already.insert(*bloom);
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

/// Private plan namespaces are reconstructible from the existing outbox. A
/// pending preparation may still own a Git worker; an outstanding order may
/// still need its checkout, so neither namespace is eligible yet.
fn prune_preview_refs(
    store: &mut dyn StoreBackend,
    source: &dyn WorkingRefPruner,
    snapshot: &Snapshot,
    already: &mut HashSet<BloomId>,
) -> rusqlite::Result<usize> {
    let plans = store.delivered_topic(Topic::QueuePrecheckPlan)?;
    if plans.is_empty() {
        return Ok(0);
    }
    let mut busy = HashSet::new();
    for nonce in store.list_order_nonces()? {
        if let Some(order) = store.lookup_order(&nonce)?
            && let Some(bloom) = Digest::from_slice(&order.bloom)
        {
            busy.insert(BloomId(bloom));
        }
    }
    let mut pruned = 0;
    let mut attempted = 0;
    for entry in plans {
        let payload = from_bytes::<QueuePrecheckPlanPayload>(&entry.payload)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("pre-check prune payload: {error}")))?;
        let bloom = BloomId(payload.bloom);
        let namespace = preview_namespace(&payload.plan);
        if already.contains(&namespace)
            || busy.contains(&bloom)
            || snapshot.blooms.get(&bloom).is_none_or(|record| is_active_unlanded(record.status))
        {
            continue;
        }
        if attempted >= JANITOR_REF_PRUNES_PER_TICK {
            break;
        }
        attempted += 1;
        match source.prune_working_refs(&namespace) {
            Ok(count) => {
                pruned += count;
                already.insert(namespace);
            }
            Err(error) => tracing::warn!(%error, "janitor: private pre-check ref prune failed; will retry"),
        }
    }
    Ok(pruned)
}

/// Whether a terminal bloom's working refs are this pass's to delete.
///
/// A landed bloom's are: mainline carries its tree and no fold reaches back
/// into its namespace again. A *superseded* bloom's are not, not yet. The
/// invariant is that a superseded bloom's working refs stay live state until
/// every bloom in its successor chain that could still adopt from it is itself
/// terminal, because adoption happens at fold time and not at supersession: a
/// successor holding an inherited claim has no candidate ref of its own for
/// that member, so the fold's `adopt_candidate` copies the predecessor's ref
/// into the successor's namespace as the fold dispatches, walking the
/// grandparents for a member carried through two supersessions. Deleted
/// beforehand, the ref exists nowhere to adopt from, the fold refuses at
/// `candidate_ref_present`, and no later tick can put it back — the successor
/// can never land, and recovery means hand-restoring refs from unreachable
/// objects.
///
/// The window looks empty because an ordinary supersede folds within a second
/// of the seal. An amend (ADR-0207) that widens a member's surface makes that
/// member re-run before the fold may dispatch, which holds the chain open for
/// however long the re-run takes — minutes — and the ten-second janitor tick
/// wins that race every time. Bloom 85bb7225 lost its predecessor's sixteen
/// refs to a tick twenty-five minutes ahead of the fold that needed them.
fn refs_are_reclaimable(snapshot: &Snapshot, bloom: &BloomId, status: BloomStatus) -> bool {
    match status {
        BloomStatus::Landed => true,
        BloomStatus::Superseded => successor_chain_is_terminal(snapshot, bloom),
        _ => false,
    }
}

/// Whether every bloom that could still adopt from `bloom` has finished: the
/// end of its supersession chain is landed or withdrawn rather than
/// active-and-unlanded.
///
/// Walks the chain rather than one link of it, for the reason the adoption
/// walks it — a member carried through two supersessions is parked under the
/// grandparent, so the live bloom reaching for it may be several links down.
/// Iterative and visited-guarded, so a journal that somehow recorded a cycle
/// costs this pass rather than the coordinator's stack.
///
/// An end this snapshot cannot resolve reads as live: a `Superseded` record
/// naming no successor, or naming one the snapshot does not hold, is a lineage
/// the pass cannot prove nobody is standing on. Keeping refs a closed chain no
/// longer needs costs ref storage the next pass reclaims once the chain
/// closes; deleting refs a fold still needs costs that bloom its landing.
fn successor_chain_is_terminal(snapshot: &Snapshot, bloom: &BloomId) -> bool {
    let mut seen = HashSet::new();
    let mut next = Some(*bloom);
    while let Some(current) = next {
        if !seen.insert(current) {
            return false;
        }
        let Some(record) = snapshot.blooms.get(&current) else {
            return false;
        };
        if is_active_unlanded(record.status) {
            return false;
        }
        if record.status != BloomStatus::Superseded {
            return true;
        }
        next = record.superseded_by;
    }
    false
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
/// This is the only disk-pressure lever, and it runs on every tick — tree and
/// text reclaim wait until the coordinator is between blooms; target
/// directories are regenerable build state and are what pressure actually
/// evicts. Occupancy is still re-read live immediately before each eviction,
/// and unattributed occupancy still refuses the pass; those guards stay
/// exactly as they are.
///
/// Reclaiming *everything* on an overage — what this did until the 2026-08-14
/// incident — makes each firing maximally destructive, and a budget set below
/// the working set makes it fire forever: the host can never get under, so
/// every tick wants to sweep and races every lane start. Evicting to the line
/// keeps the warm caches that are still inside it.
///
/// Size is the expensive part of the pass. The cheap questions — which slot
/// directories exist, which of them are free — decide whether a walk can
/// change anything; [`TargetScan`] remembers the free set the last walk was
/// taken against so an unchanged occupancy inside the scan interval does not
/// pay again.
fn sweep_targets(
    request: &SweepRequest<'_>,
    scan: &mut TargetScan,
    interval: Duration,
    now: SystemTime,
) -> (usize, usize) {
    let removed = reclaim_evicting_leftovers(request.target_base);
    let paths = slot_target_paths(request.target_base);
    // Cheap decision: occupancy at the start of the size work. The destructive
    // path re-reads live per candidate below; this sample only decides whether
    // anything is free to evict, and an unidentified child forbids every slot.
    let occupancy = (request.lanes)();
    let free = free_slots(&paths, &occupancy);
    if free.is_empty() || !scan.should_measure(&free, interval, now) {
        tracing::debug!(
            target: "aether_chassis_bloomery::janitor",
            free_slots = free.len(),
            "janitor: skipping target-dir measurement; nothing evictable or scan interval has not elapsed",
        );
        return (removed, 0);
    }

    let mut targets = measure(&paths);
    let measured = targets.len();
    scan.record(free.clone(), now);

    let total: u64 = targets.iter().map(|target| target.bytes).sum();
    let budget_bytes = request.policy.lane_target_budget_bytes;
    if total <= budget_bytes {
        return (removed, measured);
    }

    tracing::info!(
        target: "aether_chassis_bloomery::janitor",
        total_bytes = total,
        budget_bytes,
        free_slots = free.len(),
        "janitor: lane target dirs over budget",
    );
    let occupied_held: Vec<usize> =
        occupancy.slots.iter().copied().filter(|slot| targets.iter().any(|target| target.slot == *slot)).collect();
    let evicted = evict_coldest(&mut targets, request, total, budget_bytes);
    if !occupied_held.is_empty() {
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            occupied_slots = ?occupied_held,
            "janitor: occupied slots held their target dirs",
        );
    }
    (removed + evicted, measured)
}

/// Slot index and path of each `slot-<index>-target` directory under `base`.
/// A `read_dir` and a name parse — no recursive walk.
fn slot_target_paths(target_base: &Path) -> Vec<(usize, PathBuf)> {
    let Ok(entries) = fs::read_dir(target_base) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(slot) = path.file_name().and_then(|name| name.to_str()).and_then(slot_target_index) else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        paths.push((slot, path));
    }
    paths
}

/// Walk each listed slot directory for size and recency.
fn measure(paths: &[(usize, PathBuf)]) -> Vec<SlotTarget> {
    paths
        .iter()
        .map(|(slot, path)| {
            let usage = dir_usage(path);
            SlotTarget { slot: *slot, path: path.clone(), bytes: usage.bytes, used: usage.newest }
        })
        .collect()
}

fn free_slots(paths: &[(usize, PathBuf)], occupancy: &LaneOccupancy) -> BTreeSet<usize> {
    if occupancy.unattributed {
        return BTreeSet::new();
    }
    paths.iter().map(|(slot, _)| *slot).filter(|slot| !occupancy.slots.contains(slot)).collect()
}

fn evict_coldest(targets: &mut [SlotTarget], request: &SweepRequest<'_>, total: u64, budget_bytes: u64) -> usize {
    // Coldest first: the slot nobody has built in for longest is the one whose
    // loss costs the least rebuild time.
    targets.sort_by_key(|target| target.used);
    let mut held = total;
    let mut evicted = 0;
    for target in targets.iter() {
        if held <= budget_bytes {
            break;
        }
        // Live, per directory. Everything above this point — the size walk
        // especially — takes long enough for the answer to have changed.
        let occupancy = (request.lanes)();
        if occupancy.unattributed {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                "janitor: a lane child is running in an unidentified slot; leaving every target dir in place",
            );
            break;
        }
        if occupancy.slots.contains(&target.slot) {
            continue;
        }
        if evict_target_dir(&target.path) {
            held = held.saturating_sub(target.bytes);
            evicted += 1;
        }
    }
    evicted
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
pub(super) use super::records::retention_duration;

#[cfg(test)]
mod preview_tests {
    use std::cell::RefCell;

    use aether_bloomery::testing::{digest, draft, membership, splice_bloom, workpiece};
    use aether_bloomery::{
        AgentProfile, CandidateRef, CompositionContractTemplate, ConfigRegistry, ContextualInvocationTemplate,
        CoordinationPolicy, CoordinationState, ExecutionLimits, Harness, NetworkProfile, PartialHeadRepairDispatch,
        PartialHeadRepairPayload, PartialHeadRepairPlan, PrecheckPlan, ReasoningEffort, ToolPolicy, Transformation,
        VerificationMode,
    };
    use aether_data::wire::to_vec;

    use super::*;
    use crate::store::{OrderLifecycle, OutstandingOrder, SharedRunStepRow, SqliteStore};

    #[derive(Default)]
    struct Pruner(RefCell<Vec<BloomId>>);

    impl WorkingRefPruner for Pruner {
        fn prune_working_refs(&self, bloom: &BloomId) -> Result<usize, SourceError> {
            self.0.borrow_mut().push(*bloom);
            Ok(1)
        }
    }

    #[test]
    fn preview_refs_wait_for_preparation_terminal_membership_and_settled_orders() {
        let mut store = SqliteStore::open(":memory:").expect("store");
        let spec = draft(0, vec![membership("wp", 1)]).seal();
        let mut snapshot = Snapshot::default();
        splice_bloom(&mut snapshot, &spec, BloomStatus::Landed);
        let plan = PrecheckPlan { bloom: spec.id(), base: digest(0), members: Vec::new(), gate_set: digest(1) };
        let namespace = preview_namespace(&plan);
        let payload = QueuePrecheckPlanPayload { bloom: spec.id().0, plan };
        store.enqueue_topic(Topic::QueuePrecheckPlan, &to_vec(&payload).expect("payload"), None).expect("enqueue");
        let source = Pruner::default();
        let mut already = HashSet::new();
        assert_eq!(prune_preview_refs(&mut store, &source, &snapshot, &mut already).expect("pending"), 0);

        let entry = store.drain_topic(Topic::QueuePrecheckPlan).expect("pending plan").remove(0);
        store.ack_topic(Topic::QueuePrecheckPlan, entry.sequence).expect("finished preparation");
        snapshot.blooms.get_mut(&spec.id()).expect("bloom").status = BloomStatus::Sealed;
        assert_eq!(prune_preview_refs(&mut store, &source, &snapshot, &mut already).expect("active"), 0);

        snapshot.blooms.get_mut(&spec.id()).expect("bloom").status = BloomStatus::Landed;
        store
            .record_order(&OutstandingOrder {
                lifecycle: OrderLifecycle::Submitting,
                ..super::super::tests::order_for("preview", &spec.id())
            })
            .expect("submitting order");
        assert_eq!(prune_preview_refs(&mut store, &source, &snapshot, &mut already).expect("submitting"), 0);
        store.consume_order("preview").expect("settled order");
        assert_eq!(prune_preview_refs(&mut store, &source, &snapshot, &mut already).expect("terminal"), 1);
        assert_eq!(*source.0.borrow(), vec![namespace]);
        assert_ne!(namespace, spec.id());
        assert_eq!(prune_preview_refs(&mut store, &source, &snapshot, &mut already).expect("repeat"), 0);
    }

    #[test]
    fn completing_and_terminal_runs_with_open_orders_keep_their_refs() {
        let mut store = SqliteStore::open(":memory:").expect("store");
        let mut row = SharedRunRow {
            run: digest(70).as_bytes().to_vec(),
            nonce: "shared".to_owned(),
            dispatch: Vec::new(),
            lifecycle: SharedRunLifecycle::Completing,
            next_ordinal: 0,
            deadline_unix_millis: 1,
            charged: false,
            physical_cost: None,
        };
        assert!(shared_run_refs_are_live(&mut store, &row).expect("completing"));

        row.lifecycle = SharedRunLifecycle::Cancelled;
        store.record_shared_run(&row, &[]).expect("cancelled run");
        let step = SharedRunStepRow {
            run: row.run.clone(),
            ordinal: 0,
            nonce: "shared-step-0".to_owned(),
            request: None,
            descriptor: Vec::new(),
            prepared: None,
            receipt: Some(vec![1]),
            duration_millis: None,
            release_physical_run: false,
        };
        store.record_shared_run_step(&step).expect("settled step");
        store
            .record_order(&OutstandingOrder {
                lifecycle: OrderLifecycle::Submitted,
                ..super::super::tests::order_for(&step.nonce, &BloomId(digest(71)))
            })
            .expect("open order");
        assert!(shared_run_refs_are_live(&mut store, &row).expect("open order"));

        store.consume_order(&step.nonce).expect("settle order");
        assert!(!shared_run_refs_are_live(&mut store, &row).expect("terminal"));
    }

    #[test]
    fn terminal_generation_refs_wait_only_for_durable_work() {
        assert!(coordination_refs_are_live(BloomStatus::Sealed, false));
        assert!(!coordination_refs_are_live(BloomStatus::Landed, false));
        assert!(coordination_refs_are_live(BloomStatus::Landed, true));
    }

    #[test]
    fn active_and_published_partial_head_repairs_keep_their_source_pins() {
        let spec = draft(0, vec![membership("wp", 1)]).seal();
        let bloom = spec.id();
        let base = CandidateRef { tree: digest(0), checkout: digest(0) };
        let mut state = CoordinationState::new(
            CoordinationPolicy {
                verification: VerificationMode::Contextual,
                eager_integration: true,
                max_run_members: 2,
                max_serial_requests: 2,
                max_attribution_probes: 2,
                movement_budget: 1,
                reservation_millis: 1_000,
                host_class: String::from("test"),
            },
            CompositionContractTemplate {
                gate_set: digest(2),
                gate_identities: vec![String::from("verify")],
                invocation: ContextualInvocationTemplate {
                    command: String::from("verify.check"),
                    extra_inputs: Vec::new(),
                    diff_base: Some(base.checkout),
                    outputs: Vec::new(),
                    image: String::from("test"),
                    limits: ExecutionLimits { wall_clock_secs: 1 },
                    network: NetworkProfile::None,
                    description: None,
                    model: None,
                    profile: AgentProfile {
                        harness: Harness::Grok,
                        model: String::from("test"),
                        effort: ReasoningEffort::Low,
                        tools: ToolPolicy::None,
                    },
                    configs: ConfigRegistry::default(),
                },
                environment: digest(3),
                host_class: digest(4),
            },
            bloom,
            base,
            vec![(workpiece("wp"), digest(1))],
        );
        let plan = PartialHeadRepairPlan {
            bloom,
            generation: state.integration.generation.digest(),
            head: state.integration.head.clone(),
            inputs: Vec::new(),
            evidence: digest(5),
            attempt: 0,
        };
        state.partial_head_repair = Some(plan.clone());
        let mut snapshot = Snapshot::default();
        splice_bloom(&mut snapshot, &spec, BloomStatus::Sealed);
        snapshot.blooms.get_mut(&bloom).expect("bloom").coordination = Some(Box::new(state));
        let mut store = SqliteStore::open(":memory:").expect("store");
        let payload = PartialHeadRepairPayload {
            dispatch: PartialHeadRepairDispatch {
                plan: plan.clone(),
                transformation: Transformation {
                    command: String::from("refine"),
                    inputs: vec![plan.head.candidate.tree],
                    checkout: plan.head.candidate.checkout,
                    diff_base: Some(base.checkout),
                    outputs: Vec::new(),
                    image: String::from("test"),
                    limits: ExecutionLimits { wall_clock_secs: 1 },
                    network: NetworkProfile::None,
                    description: None,
                    model: None,
                },
                scope_revision: digest(0),
                profile: AgentProfile {
                    harness: Harness::Grok,
                    model: String::from("test"),
                    effort: ReasoningEffort::Low,
                    tools: ToolPolicy::None,
                },
                configs: ConfigRegistry::default(),
            },
        };
        store.enqueue_topic(Topic::PartialHeadRepair, &to_vec(&payload).expect("payload"), None).expect("enqueue");
        let entry = store.drain_topic(Topic::PartialHeadRepair).expect("pending repair").remove(0);
        store.ack_topic(Topic::PartialHeadRepair, entry.sequence).expect("delivered repair");
        let source = Pruner::default();
        let mut already = HashSet::new();

        let protected = protected_partial_head_repairs(&mut store, &snapshot).expect("active repair");
        assert!(protected.contains(&plan.digest()));
        assert_eq!(
            prune_partial_head_repair_refs(&mut store, &source, &protected, &mut already).expect("active repair prune"),
            0,
        );

        {
            let state = snapshot.blooms.get_mut(&bloom).expect("bloom").coordination.as_mut().expect("coordination");
            state.partial_head_repair = None;
            state.integration.head.plan = plan.digest();
        }
        assert!(
            protected_partial_head_repairs(&mut store, &snapshot).expect("published repair").contains(&plan.digest())
        );
        snapshot
            .blooms
            .get_mut(&bloom)
            .expect("bloom")
            .coordination
            .as_mut()
            .expect("coordination")
            .integration
            .head
            .plan = Digest::default();
        assert_eq!(
            prune_partial_head_repair_refs(&mut store, &source, &HashSet::new(), &mut already)
                .expect("retired repair prune"),
            1,
        );
        assert_eq!(*source.0.borrow(), vec![partial_head_repair_namespace(plan.digest())]);
    }
}
