//! The local-process executor backend: an in-process registry of tracked runs
//! over the [`TransformRunner`] spawn seam, and its [`ExecutorBackend`] impl.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf, absolute};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_bloomery::digest::ContentAddressed;
use aether_bloomery::{
    BackendObjectId, CandidateRef, Conclusion, ConfigRegistry, ConfigScopes, Digest, EvidenceRef, ExecutionStatus,
    ExecutorBackend, Nonce, PriceTable, ResolvedModel, SharedCorrespondence, StageVerdict, StudyCost, Transformation,
    VerifyFailureSet, WorkHandle, WorkOrder, digest_of, is_model_lane,
};
use aether_bloomery_github::parse_study;
use aether_data::Kind;
use aether_data::wire::from_bytes;
use serde::Serialize;
use std::fs;

use super::error::LocalExecutorError;
use super::lane_program::LaneProgram;
use super::orphan::OrphanedRun;
use super::process_runner::{CaptureIdentity, ProcessTransformRunner};
use super::runner::{RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::bloomery::CONSTRUCT_IMPLEMENT_COMMAND;
use crate::bloomery::CoordinatorConfig;
use crate::bloomery::executor::{LaneOccupancy, OutstandingDispatch, ReconcileLanes, ReconcileReport};
use crate::bloomery::intake::NameEvidenceClaims;
use crate::bloomery::triage::MAX_TRIAGED_DIFF_BYTES;
use crate::session::SessionConfig;
use crate::store::{SqliteStore, StoreBackend};

/// The suffix distinguishing a run's evidence directory from the lane slot
/// checkouts under the same base dir. Evidence stays per dispatch (nonce-keyed):
/// it is what that one attempt produced.
const EVIDENCE_SUFFIX: &str = "-evidence";

/// The prefix a lane slot's canonical checkout directory carries under the
/// scratch root, completed by the slot's index (`slot-0`, `slot-1`, …).
///
/// The build path is what makes this a name rather than a nonce (#4904).
/// `sccache` keys every compilation partly by the paths cargo names on the
/// `rustc` invocation — `--out-dir`, `-L dependency=…` — so a dispatch that
/// builds at a path no dispatch built at before misses the cache on its whole
/// dependency tree, however much of that tree an earlier lane already compiled.
/// A slot's dispatches all build at the slot's own path, so the second one hits
/// what the first paid for.
const SLOT_PREFIX: &str = "slot-";

/// The suffix a lane slot's cargo target directory carries, completing the slot's
/// own name (`slot-0-target`, `slot-1-target`) under the target base.
///
/// Per slot for the reason the checkout is (#4912): cargo takes an exclusive lock
/// on a build directory, so lanes sharing one build strictly one at a time
/// however many slots the ceiling allows, and its fingerprints are keyed by
/// source path, so a directory shared across divergent checkouts both grows
/// without bound and can surface a dependency last compiled from another slot's
/// source as a failure that reads as a regression.
///
/// A **sibling** of the checkout rather than a directory inside it. A dispatch
/// resets its slot with `git clean --force --force -d -x` (see
/// `materialize_checkout`), which removes ignored files — an in-tree `target`
/// would be deleted once per dispatch, and the warm dependency tree that makes a
/// repair lap recompile nine crates instead of ninety-six is exactly what would
/// be lost.
const TARGET_SUFFIX: &str = "-target";

/// How many build jobs a lane's cargo invocations run at once when nothing
/// configures it — the seam default, mirroring
/// [`CoordinatorConfig::lane_build_jobs`]'s, which is what production resolves.
const DEFAULT_LANE_BUILD_JOBS: usize = 8;

/// The file a dispatch records its lane slot in, inside its own evidence
/// directory — the durable half of the slot assignment, read back by boot
/// reconciliation (see [`recorded_slot`]).
const SLOT_RECORD: &str = "slot";

/// One tracked run: the spawned child, the lane slot it holds, where its
/// evidence lands, and the digest the returning evidence must bind to.
struct Run {
    process: Box<dyn RunProcess>,
    // The lane slot this run occupies, released on its terminal path (cancel, or
    // evidence consumed) so the next dispatch can build at that slot's path.
    // `None` for a run re-adopted at boot whose slot could not be recovered:
    // this process cannot name the checkout it is running in, and guessing would
    // hand another dispatch's live build path to a stranger.
    slot: Option<usize>,
    // The slot checkout the run builds in — the same path every dispatch in that
    // slot uses, reset to the dispatch's own tree as it starts rather than
    // created fresh. `None` alongside a `None` slot.
    worktree_dir: Option<PathBuf>,
    evidence_dir: PathBuf,
    // The digest the intake broker binds the evidence to, per `evidence_subject`.
    subject: Digest,
    // Which lane-specific evidence gates this run's verdict rides, decided from
    // the order's command.
    gates: LaneGates,
    // The session-reuse decision taken at spawn — stamped onto evidence and
    // used to deposit the session the attempt produced. `None` when this
    // backend has no pool or the lane is not a Claude model lap.
    reuse: Option<super::ReusePlan>,
}

/// Which lane-specific evidence gates a command's run rides.
///
/// Derived in one place because two paths stamp it onto a run — `submit` for a
/// fresh dispatch and `reconcile` for one re-adopted at boot — and a run whose
/// gates disagree with its lane reads its own evidence by the wrong rule.
#[derive(Clone, Copy)]
struct LaneGates {
    // Whether this run is the model-driven construct lane. The completion gate is
    // lane-specific: a construct run's verdict demands a substantive conclusion
    // (#3596), a verify run's rides its stamped `status`, so the gate must know
    // which lane produced the evidence — and must know it even when the evidence
    // bytes do not decode (fail-closed).
    is_construct: bool,
    // Whether the evidence body belongs to a mechanical verify lane and must
    // decode ADR-0178's `failed_verifiers` field.
    is_verify: bool,
}

impl LaneGates {
    fn of(command: &str) -> Self {
        Self { is_construct: command == CONSTRUCT_IMPLEMENT_COMMAND, is_verify: command.starts_with("verify.") }
    }
}

/// One dispatch that has been accepted but not yet spawned, waiting for a lane
/// slot under the backend's concurrency ceiling.
///
/// Owns everything [`TransformRunner::start`] is handed, because the borrowed
/// [`WorkOrder`] is long gone by the time a slot frees, plus the two fields the
/// tracked [`Run`] it becomes carries.
struct PendingRun {
    nonce: String,
    command: String,
    checkout_hex: String,
    diff_base_hex: Option<String>,
    evidence_dir: PathBuf,
    profile: Option<ResolvedModel>,
    task: Option<String>,
    subject: Digest,
    gates: LaneGates,
}

impl PendingRun {
    // The spawn request, over the slot checkout the dispatch was handed and the
    // target directory beside it. Neither build path is resolved with the rest of
    // the dispatch: both belong to the slot, and which slot this run gets is
    // decided when one frees.
    fn spec<'a>(
        &'a self,
        worktree_dir: &'a Path,
        target_dir: &'a Path,
        build_jobs: usize,
        resume: Option<&'a str>,
    ) -> RunSpec<'a> {
        RunSpec {
            command: &self.command,
            checkout_hex: &self.checkout_hex,
            diff_base_hex: self.diff_base_hex.as_deref(),
            worktree_dir,
            target_dir,
            build_jobs,
            evidence_dir: &self.evidence_dir,
            nonce: &self.nonce,
            harness: self.profile.as_ref().map(|resolved| resolved.harness.as_str()),
            model: self.profile.as_ref().map(|resolved| resolved.model.as_str()),
            effort: self.profile.as_ref().map(|resolved| resolved.effort.as_str()),
            task: self.task.as_deref(),
            resume,
        }
    }
}

/// What the backend tracks: the runs it has spawned, and the dispatches waiting
/// for a slot to open under the concurrency ceiling.
///
/// One lock over both, because the decision a submit makes — start now, or wait
/// — reads them together, and a ceiling enforced across two locks is a ceiling
/// two submits can walk through at once.
#[derive(Default)]
struct Registry {
    runs: HashMap<String, Run>,
    waiting: VecDeque<PendingRun>,
    // Slots held by a `start` that is in flight. The spawn shells out to git and
    // must not hold this lock, so a reservation stands in for the run between the
    // decision to start it and the registry entry it becomes.
    starting: usize,
    // Which slot indices are spoken for right now — the allocator behind the
    // canonical build paths. Held from the moment a dispatch is handed a slot
    // until the run leaves the registry, because the slot's checkout is what the
    // dispatch is building in and the next claimant resets it.
    slots: HashSet<usize>,
}

impl Registry {
    // How many lane slots are spoken for: the spawned runs, plus the starts that
    // have not yet become one.
    fn occupied(&self) -> usize {
        self.runs.len() + self.starting
    }

    // Claim the lowest unheld slot index.
    //
    // Lowest rather than next-in-sequence, and that is the whole point: a
    // counter would mint a fresh path per dispatch — exactly the arrangement
    // that keeps the compiler cache at a 0% hit rate — while reusing the lowest
    // free index keeps a host's builds inside `0..ceiling` paths forever.
    //
    // One of `0..=len` is always free, so the search is total.
    fn claim_lowest_free_slot(&mut self) -> usize {
        let slot = (0..=self.slots.len()).find(|index| !self.slots.contains(index)).unwrap_or(self.slots.len());
        self.slots.insert(slot);
        slot
    }

    // Claim one named slot, reporting whether it was free. Boot reconciliation's
    // path: a re-adopted run must get back the slot it was dispatched in, not
    // whichever one happens to be lowest.
    fn claim_slot(&mut self, slot: usize) -> bool {
        self.slots.insert(slot)
    }

    // Hand a terminal run's slot back, so the next dispatch builds at its path.
    fn release_slot(&mut self, slot: Option<usize>) {
        if let Some(slot) = slot {
            self.slots.remove(&slot);
        }
    }
}

/// The digest a run's returning evidence binds to: the order's subject input —
/// the scope-revision digest the broker displayed — falling back to the checkout
/// only for a malformed order that carries no input.
///
/// Not the checkout target. The two are distinct axes: the checkout is the tree
/// the work runs on, the subject is what the evidence is about, and binding to
/// the checkout would refuse at intake as a digest mismatch.
fn evidence_subject(transformation: &Transformation) -> Digest {
    transformation.inputs.first().copied().unwrap_or(transformation.checkout)
}

/// The local-process executor backend: an in-process registry of tracked runs
/// keyed by nonce, over a [`TransformRunner`] spawn seam.
pub struct LocalExecutor {
    runner: Arc<dyn TransformRunner>,
    correspondence: SharedCorrespondence,
    base_dir: PathBuf,
    registry: Mutex<Registry>,
    // How many lane children may run at once. `usize::MAX` is the unthrottled
    // seam default; production resolves it from config through
    // `with_max_concurrent_lanes`.
    max_concurrent_lanes: usize,
    // Where the per-slot cargo target directories live. The scratch root by
    // default — a target dir is then the sibling of the checkout it serves — and a
    // configured volume when the host names one it would rather build on.
    target_base: PathBuf,
    // How many build jobs one lane's cargo invocations may run at once, exported
    // as `CARGO_BUILD_JOBS`. `0` leaves cargo's own default (one job per core).
    build_jobs: usize,
    // The store the captured candidate's commit message is filed in, keyed by the
    // member the run's order names. `None` for a backend built without one (the
    // seam tests), which simply files nothing.
    //
    // Its own connection to the coordinator's store file, exactly as
    // `SqliteCorrespondence` and the dispatch reactor open theirs — the WAL
    // journal serializes the rare concurrent write. Behind a `Mutex` because the
    // port's methods take `&self` while the store writes through `&mut`.
    messages: Option<Mutex<SqliteStore>>,
    // The session-reuse pool this backend consumes. `None` for a backend built
    // without one (most seam tests), which launches every lap cold.
    sessions: Option<super::SessionReuse>,
}

impl LocalExecutor {
    /// Build a backend over an explicit spawn seam — the seam tests drive with a
    /// stub runner, and [`from_config`](Self::from_config) drives with the
    /// production [`ProcessTransformRunner`]. `base_dir` is the scratch root;
    /// each run writes its evidence to `base_dir/<nonce>-evidence` and builds in
    /// the canonical checkout of the lane slot it holds, `base_dir/slot-<index>`.
    /// Unthrottled — every submit spawns immediately until
    /// [`with_max_concurrent_lanes`](Self::with_max_concurrent_lanes) sets a
    /// ceiling, which is what [`from_config`](Self::from_config) does.
    #[must_use]
    pub fn new(
        runner: Arc<dyn TransformRunner>,
        correspondence: SharedCorrespondence,
        base_dir: impl Into<PathBuf>,
    ) -> Self {
        let base_dir = base_dir.into();
        Self {
            runner,
            correspondence,
            target_base: base_dir.clone(),
            base_dir,
            registry: Mutex::new(Registry::default()),
            max_concurrent_lanes: usize::MAX,
            build_jobs: DEFAULT_LANE_BUILD_JOBS,
            messages: None,
            sessions: None,
        }
    }

    /// Put the per-slot cargo target directories under `configured` instead of
    /// beside the slot checkouts, and cap one lane's cargo invocations at
    /// `build_jobs` (`0` leaves cargo's own default).
    ///
    /// An empty `configured` keeps the default arrangement, and so does one that
    /// resolves inside a slot checkout — see `usable_target_base`, which is
    /// where the refusal and its reason live.
    #[must_use]
    pub fn with_lane_build(mut self, configured: &str, build_jobs: usize) -> Self {
        self.target_base = usable_target_base(configured, &self.base_dir);
        self.build_jobs = build_jobs;
        self
    }

    /// Cap how many lane children this backend runs at once.
    ///
    /// Each lane is a full cargo build with its own throwaway target dir, and a
    /// seal fans out one dispatch per member, so an uncapped backend turns member
    /// count directly into simultaneous builds racing the same CPU and disk.
    /// Dispatches past the ceiling wait in submission order and start as running
    /// lanes finish — a queue, never a refusal: every dispatch is acked as
    /// submitted either way, so the reducer's view of it is unchanged and no order
    /// re-drains.
    ///
    /// The ceiling is per backend rather than per bloom: member lanes, aggregate
    /// lanes, and the runs re-adopted at boot all count against the same slots.
    ///
    /// A ceiling of zero would park every dispatch forever, so it resolves to one
    /// — the smallest ceiling that still makes progress.
    #[must_use]
    pub fn with_max_concurrent_lanes(mut self, ceiling: usize) -> Self {
        if ceiling == 0 {
            tracing::warn!("local executor backend: a lane ceiling of zero would start nothing; using one");
        }
        self.max_concurrent_lanes = ceiling.max(1);
        self
    }

    /// Mount the store a captured candidate's commit message is filed in. A
    /// backend without one captures exactly as before and files nothing, so the
    /// seam tests that do not care about the message need not open a store.
    #[must_use]
    pub fn with_message_store(mut self, store: SqliteStore) -> Self {
        self.messages = Some(Mutex::new(store));
        self
    }

    /// Mount the session-reuse pool a retry lap acquires from. A backend
    /// without one launches every attempt cold, so the seam tests that do
    /// not care about resume need not open a pool.
    #[must_use]
    pub fn with_session_reuse(mut self, sessions: super::SessionReuse) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// Build the production backend from resolved config: the real git + cargo
    /// [`ProcessTransformRunner`], the shared `correspondence` the checkout
    /// resolves through, and the config'd scratch-worktree base dir. The model a
    /// run executes under is not config — it rides each order as the resolved
    /// agent profile the host overlaid at dispatch (ADR-0149 §The line).
    ///
    /// `session` is the same [`SessionConfig`] the chassis mounts on
    /// [`SessionPoolCapability`](crate::session::SessionPoolCapability): the
    /// executor consumes that pool rather than opening a second one with
    /// hardcoded knobs.
    #[must_use]
    pub fn from_config(
        config: &CoordinatorConfig,
        correspondence: SharedCorrespondence,
        session: &SessionConfig,
    ) -> Self {
        let identity = CaptureIdentity { name: config.operator_name.clone(), email: config.operator_email.clone() };

        let backend = Self::new(
            Arc::new(ProcessTransformRunner::new(identity, LaneProgram::parse(&config.local_lane_program))),
            correspondence,
            config.local_worktree_base.clone(),
        )
        .with_max_concurrent_lanes(config.max_concurrent_lanes)
        .with_lane_build(&config.lane_target_base, config.lane_build_jobs);
        let backend = match super::SessionReuse::from_config(session) {
            Ok(sessions) => backend.with_session_reuse(sessions),
            Err(error) => {
                tracing::warn!(
                    path = %session.store_path(),
                    %error,
                    "local executor backend: session pool unavailable; every lap launches cold",
                );
                backend
            }
        };
        // A store this backend cannot open costs the landing proposal its
        // authored title, never a candidate: the capture still commits and the
        // land path still falls back, so the miss warns rather than failing boot.
        match SqliteStore::open(&config.store_path) {
            Ok(store) => backend.with_message_store(store),
            Err(error) => {
                tracing::warn!(
                    store = %config.store_path,
                    %error,
                    "local executor backend: commit-message store unavailable; captured candidates will name no message",
                );
                backend
            }
        }
    }

    // Lock the registry, recovering the guard on a poisoned mutex rather than
    // panicking — a backend is long-lived behind an Arc and a poisoned lock
    // should degrade to best-effort, not take the whole coordinator down.
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // The evidence output dir a run at `nonce` owns — per dispatch, because it
    // holds what that one attempt produced.
    //
    // Resolved absolute against the coordinator's own cwd. The child runs with
    // `current_dir(worktree_dir)`, so a relative `--out` (the config default
    // `local_worktree_base` ships relative) would resolve against the *child's*
    // cwd — the slot checkout — while `stream_evidence` reads `evidence_dir`
    // against the *coordinator's* cwd; the two diverge and the intake polls a path
    // the run never wrote, forever. `std::path::absolute` is a lexical cwd-join
    // that does not require the path to exist (unlike `canonicalize`).
    //
    // The single spelling of the nonce→path convention, because the boot
    // reconciliation reads it backwards: it recovers a run from a directory under
    // `base_dir`, which only works while both sides agree on the layout.
    fn evidence_dir(&self, nonce: &str) -> io::Result<PathBuf> {
        absolute(self.base_dir.join(format!("{nonce}{EVIDENCE_SUFFIX}")))
    }

    // The canonical checkout one lane slot builds in, reused by every dispatch
    // that holds the slot. Absolute for the reason the evidence dir is.
    fn slot_dir(&self, slot: usize) -> io::Result<PathBuf> {
        absolute(self.base_dir.join(format!("{SLOT_PREFIX}{slot}")))
    }

    // The cargo target directory one lane slot builds into, reused by every
    // dispatch that holds the slot exactly as its checkout is. Absolute for the
    // reason the other two are: the child runs with `current_dir(worktree_dir)`,
    // so a relative `CARGO_TARGET_DIR` would land inside the checkout — the one
    // place it must never be.
    fn slot_target_dir(&self, slot: usize) -> io::Result<PathBuf> {
        absolute(self.target_base.join(format!("{SLOT_PREFIX}{slot}{TARGET_SUFFIX}")))
    }

    // Drop a terminal run from the registry and hand its slot back, so the next
    // dispatch can build at that slot's path. The checkout itself stays: it is
    // the slot's, not the run's, and the dispatch that takes the slot next resets
    // it to its own tree before building (see `ProcessTransformRunner::start`).
    fn retire(&self, nonce: &str) {
        let mut registry = self.lock();
        let slot = registry.runs.remove(nonce).and_then(|run| run.slot);
        registry.release_slot(slot);
    }

    // Resolve one work order into the spawn it will become.
    //
    // Every fallible step of a dispatch lives here rather than at the spawn, so a
    // dispatch that waits for a lane slot has already had its checkout, its diff
    // base, and its paths resolved: what is left to fail later is the spawn alone,
    // and the caller that could have re-driven the order is still on the stack for
    // everything else.
    fn prepare(&self, order: &WorkOrder) -> Result<PendingRun, LocalExecutorError> {
        let nonce = order.nonce.0.clone();
        let evidence_dir = self.evidence_dir(&nonce).map_err(LocalExecutorError::Io)?;
        // Resolve the sealed checkout digest to its real backend object through the
        // correspondence store (ADR-0150) — the `git worktree add` target — rather
        // than hex-punning the digest into a name git cannot resolve. The opaque
        // bytes become Git text only here, at the argv the runner shells out with.
        let checkout_hex = render_object_hex(
            &self
                .correspondence
                .resolve_backend_object(&order.transformation.checkout)?
                .ok_or_else(|| LocalExecutorError::UnresolvedCheckout(order.nonce.clone()))?,
        );
        // The diff source rides the work order (#4723) and resolves the same way:
        // an order that names one is judged over the range `base..checkout`, one
        // that does not is judged over the working tree. Refused when it does not
        // resolve rather than silently omitted — the omission is invisible at the
        // lane, which then reads an empty working-tree diff as an empty candidate.
        let diff_base_hex = order
            .transformation
            .diff_base
            .map(|base| {
                self.correspondence
                    .resolve_backend_object(&base)?
                    .ok_or_else(|| LocalExecutorError::UnresolvedDiffBase(order.nonce.clone()))
                    .map(|object| render_object_hex(&object))
            })
            .transpose()?;

        // Harness/model/effort/task ride the model-driven lanes (construct and
        // the review critic), mirroring `transform-model.yml`'s argv; a verify
        // lane ignores them. The gates stay narrower — `is_construct` selects the
        // construct-specific evidence gate (substantive-conclusion, #3596), which
        // the review lane's `status`-stamped evidence must not ride.
        let gates = LaneGates::of(&order.transformation.command);
        let is_model_lane = is_model_lane(&order.transformation.command);
        // The stage's resolved agent profile, overlaid onto the order by the
        // dispatching host (ADR-0149 §The line) — never a backend-local config
        // knob, which would let a run's model diverge from the profile its bloom
        // sealed and the receipt attests. An order that carries none names no
        // model, and the child falls back to the operator's ambient default.
        let profile = is_model_lane.then_some(order.transformation.model.as_ref()).flatten();

        Ok(PendingRun {
            nonce,
            command: order.transformation.command.clone(),
            checkout_hex,
            diff_base_hex,
            evidence_dir,
            profile: profile.cloned(),
            // The work-order description rides the order's transformation (#3595),
            // populated at dispatch from durable state; the model lanes name it
            // (the critic judges the candidate against it), mirroring the
            // model/effort gate.
            task: is_model_lane.then_some(order.transformation.description.clone()).flatten(),
            subject: evidence_subject(&order.transformation),
            gates,
        })
    }

    // Claim a lane slot for a dispatch that may start right now, or report that it
    // has to wait.
    //
    // A dispatch already waiting has the prior claim on a free slot, so a fresh one
    // never overtakes it — the submission order the queue promises holds even in
    // the window between a slot freeing and the pump handing it out.
    fn reserve_slot(&self) -> Option<usize> {
        let mut registry = self.lock();
        if !registry.waiting.is_empty() || registry.occupied() >= self.max_concurrent_lanes {
            return None;
        }
        registry.starting += 1;
        Some(registry.claim_lowest_free_slot())
    }

    // Park a dispatch behind the ceiling until a slot frees, saying so in the log.
    //
    // A wide bloom fans out one dispatch per member and every one of them acks as
    // submitted, so without this line a queued lane is indistinguishable from a
    // wedged one: nothing is running, nothing is failing, and nothing says why.
    fn enqueue(&self, pending: PendingRun) {
        let nonce = pending.nonce.clone();
        let depth = {
            let mut registry = self.lock();
            registry.waiting.push_back(pending);
            registry.waiting.len()
        };
        tracing::info!(
            %nonce,
            queue_depth = depth,
            ceiling = self.max_concurrent_lanes,
            "local executor backend: lane ceiling reached; dispatch is queued and starts when a lane finishes",
        );
    }

    // Spawn a dispatch into the lane slot it reserved, turning the reservation
    // into a tracked run. The spawn itself runs off the registry lock — it shells
    // out to git — and the reservation is what keeps the slot it is spending, and
    // the build path that comes with it, from being handed to anyone else
    // meanwhile. A spawn that fails hands the slot straight back.
    fn record_session_reuse(
        &self,
        evidence_path: &Path,
        bytes: &[u8],
        plan: &super::ReusePlan,
        lifecycle: RunLifecycle,
    ) -> Vec<u8> {
        let actual_turns = super::session_reuse::parse_actual_turns(bytes);
        let bytes = super::session_reuse::stamp_reuse(bytes, plan, actual_turns);
        let _ = fs::write(evidence_path, &bytes);
        if matches!(lifecycle, RunLifecycle::Exited { .. })
            && let Some(session_id) = super::session_reuse::parse_session_id(&bytes)
            && let Some(sessions) = self.sessions.as_ref()
        {
            sessions.deposit(plan, &session_id, super::session_reuse::parse_context_tokens(&bytes).unwrap_or(0));
        }
        bytes
    }

    fn acquire_reuse(&self, pending: &PendingRun, worktree_dir: &Path) -> Option<super::ReusePlan> {
        let sessions = self.sessions.as_ref()?;
        // Every harness keys into the pool the same way — the arm comes from
        // the sealed row for the resolved model, never from the harness name.
        let profile = pending.profile.as_ref()?;
        // Command + description: a critic that shares the construct lap's
        // model, effort, and work-order text must not resume the constructor.
        let task = super::session_reuse::pool_task(&pending.command, pending.task.as_deref());
        let prices = self.sealed_prices(&pending.nonce);
        Some(sessions.acquire(&super::AcquireRequest {
            model: &profile.model,
            effort: profile.effort.as_str(),
            task: &task,
            worktree: worktree_dir,
            prices: Some(&prices),
        }))
    }

    // The bloom's sealed price table, looked up from the outstanding order
    // the reactor recorded. An empty table (no store, no sealed rates, a
    // decode miss) leaves the seed to decide — resume on lap 2 — rather
    // than inventing rates.
    fn sealed_prices(&self, nonce: &str) -> PriceTable {
        let Some(messages) = self.messages.as_ref() else {
            return PriceTable::default();
        };
        let mut store = messages.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(Some(order)) = store.lookup_order(nonce) else {
            return PriceTable::default();
        };
        let Ok(registry) = from_bytes::<ConfigRegistry>(&order.configs) else {
            return PriceTable::default();
        };
        let Some(address) = ConfigScopes::bloom_wide(&registry).address::<PriceTable>() else {
            return PriceTable::default();
        };
        let Ok(Some((kind, bytes))) = store.lookup_config(address.as_bytes()) else {
            return PriceTable::default();
        };
        if kind != PriceTable::NAME {
            return PriceTable::default();
        }
        let table = match PriceTable::from_sealed(&bytes) {
            aether_bloomery::SealedPriceTable::Current(table) => table,
            aether_bloomery::SealedPriceTable::PreMigration | aether_bloomery::SealedPriceTable::Unresolvable => {
                PriceTable::default()
            }
        };
        drop(store);
        table
    }

    fn start_reserved(&self, pending: PendingRun, slot: usize) -> Result<(), LocalExecutorError> {
        let worktree_dir = self.slot_dir(slot).map_err(LocalExecutorError::Io)?;
        let target_dir = self.slot_target_dir(slot).map_err(LocalExecutorError::Io)?;
        record_slot(&pending.evidence_dir, slot);
        let reuse = self.acquire_reuse(&pending, &worktree_dir);
        let resume = reuse.as_ref().and_then(|plan| plan.resume.as_deref());
        let started = self.runner.start(&pending.spec(&worktree_dir, &target_dir, self.build_jobs, resume));

        let mut registry = self.lock();
        registry.starting -= 1;
        let outcome = match started {
            Ok(process) => {
                registry.runs.insert(
                    pending.nonce,
                    Run {
                        process,
                        slot: Some(slot),
                        worktree_dir: Some(worktree_dir),
                        evidence_dir: pending.evidence_dir,
                        subject: pending.subject,
                        gates: pending.gates,
                        reuse,
                    },
                );
                Ok(())
            }
            Err(error) => {
                registry.release_slot(Some(slot));
                Err(error)
            }
        };
        drop(registry);
        outcome
    }

    // Hand every free lane slot to the dispatches waiting for one, in submission
    // order. Called wherever a run leaves the registry, which is the only place a
    // slot frees.
    //
    // A queued start that fails has no caller left to refuse it — its dispatch
    // acked as submitted cycles ago — so it is logged and dropped rather than
    // retried: the order stays outstanding with no run behind it and resolves
    // through the reactor's deadline sweep, and a head that fails every time can
    // never block the lanes queued behind it.
    fn pump(&self) {
        while let Some((pending, slot)) = self.take_waiting() {
            let nonce = pending.nonce.clone();
            if let Err(error) = self.start_reserved(pending, slot) {
                tracing::error!(
                    %nonce,
                    %error,
                    "local executor backend: a queued dispatch failed to start; its order will expire at its deadline",
                );
            }
        }
    }

    // Take the next waiting dispatch against a free slot, reserving that slot.
    // `None` when the ceiling is full or nothing is waiting — either way the pump
    // above has nothing left to do.
    fn take_waiting(&self) -> Option<(PendingRun, usize)> {
        let mut registry = self.lock();
        if registry.occupied() >= self.max_concurrent_lanes {
            return None;
        }
        // Reserved as it is taken: the slot is spent from here, not from whenever
        // the spawn it is handed to returns.
        let pending = registry.waiting.pop_front()?;
        registry.starting += 1;
        Some((pending, registry.claim_lowest_free_slot()))
    }

    // Re-adopt one live order's run, if the previous process left a local footprint
    // for it. Returns whether it was re-adopted.
    //
    // The evidence dir is that footprint. It is created first — before the
    // checkout, before the spawn — so a dispatch that reached the local lane at
    // all has one, and it is the part the order is waiting on: re-adopting on it
    // recovers the run's verdict.
    //
    // The slot the dispatch recorded there comes back with it. Which slot matters
    // because the checkout is the slot's rather than the run's: without the
    // record, the next dispatch would claim that slot and reset the very tree a
    // surviving child is building in. A footprint that recorded no usable slot is
    // re-adopted with none, so this process never releases or resets a checkout it
    // cannot prove belongs to the run.
    //
    // Never replaces a tracked entry: an owned run's `Box<dyn RunProcess>` is the
    // only handle on its child, and swapping it for an orphan would silently retire
    // the ability to kill that child.
    //
    // A re-adoption enters the registry directly, so it occupies a lane slot like
    // any other run — and past the ceiling when a previous process left more live
    // runs than this one allows, which is the honest reading: those children are
    // already running, and what the ceiling can still do is start nothing new until
    // they drain.
    fn readopt(&self, dispatch: &OutstandingDispatch) -> bool {
        let nonce = &dispatch.nonce;
        let evidence_dir = match self.evidence_dir(&nonce.0) {
            Ok(evidence_dir) => evidence_dir,
            Err(error) => {
                tracing::warn!(
                    nonce = %nonce.0,
                    %error,
                    "local executor backend: could not resolve a re-adopted run's paths",
                );
                return false;
            }
        };
        if !evidence_dir.exists() {
            return false;
        }
        let mut registry = self.lock();
        if registry.runs.contains_key(&nonce.0) {
            return false;
        }
        let slot = recorded_slot(&evidence_dir).filter(|slot| registry.claim_slot(*slot));
        registry.runs.insert(
            nonce.0.clone(),
            Run {
                process: Box::new(OrphanedRun::new(nonce.clone(), &evidence_dir)),
                slot,
                worktree_dir: slot.and_then(|slot| self.slot_dir(slot).ok()),
                evidence_dir,
                subject: evidence_subject(&dispatch.transformation),
                gates: LaneGates::of(&dispatch.transformation.command),
                reuse: None,
            },
        );
        true
    }

    // Reclaim the nonce-keyed scratch checkouts belonging to no live order,
    // returning how many were reclaimed.
    //
    // The candidates are the repo's *registered* worktrees, not the scratch root's
    // directory listing. The listing cannot tell this backend's checkouts from
    // anything else a deployment keeps under the configured root — and it is a
    // configured root, so the sweep must not assume it owns everything below it;
    // acting on a directory listing means deleting an operator's files on the
    // strength of where they sat. A registration, filtered to direct children of
    // the root, is positive proof of a checkout this backend created.
    //
    // A slot checkout is never one of them. It belongs to the lane slot rather
    // than to any one order, which is what makes its path canonical and its
    // compilations cacheable; removing it because no order names it would undo
    // that on every boot, and would remove a live re-adopted run's tree along the
    // way. It is bounded (one per slot), and the dispatch that takes the slot next
    // resets it, so it needs no sweep. What remains sweepable is a run directory
    // keyed by a nonce — what a coordinator from before this layout left behind.
    //
    // What that leaves behind is a dispatch that died between creating its
    // directory and registering the worktree. That is bounded litter, and
    // `reclaim_worktree_path` clears it when the path is next dispatched into — a
    // fair trade against a sweep that could delete something it does not own.
    fn sweep_abandoned(&self, live: &HashSet<&str>) -> usize {
        // Canonical, because git reports canonical paths and the configured root
        // may be relative or reached through a symlink. An absent root is the
        // ordinary nothing-dispatched-locally-yet case.
        let Ok(base) = fs::canonicalize(&self.base_dir) else {
            return 0;
        };
        let registered = match self.runner.registered_worktrees() {
            Ok(registered) => registered,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "local executor backend: worktree registrations unreadable; abandoned checkouts not reclaimed",
                );
                return 0;
            }
        };

        let mut reclaimed = 0;
        for worktree in registered {
            let Some(nonce) = scratch_nonce_of(&base, &worktree) else {
                continue;
            };
            if is_slot_directory(&nonce) || live.contains(nonce.as_str()) {
                continue;
            }
            tracing::info!(
                %nonce,
                worktree = %worktree.display(),
                "local executor backend: reclaiming a scratch checkout left by an order that is no longer outstanding",
            );
            self.reclaim_checkout(&worktree);
            // The run's evidence dir is the checkout's sibling by construction, so
            // reclaiming one reclaims the pair rather than leaving half a run behind.
            remove_abandoned(&self.base_dir.join(format!("{nonce}{EVIDENCE_SUFFIX}")));
            reclaimed += 1;
        }
        reclaimed
    }

    // Drop an abandoned checkout through the runner seam, which removes the
    // directory and the `git worktree` registration together. Falls back to a plain
    // directory removal when git refuses the path, so a registration whose
    // directory git will not take back still stops costing disk.
    fn reclaim_checkout(&self, dir: &Path) {
        if let Err(error) = self.runner.release(dir) {
            tracing::warn!(
                worktree = %dir.display(),
                %error,
                "local executor backend: git refused an abandoned checkout; removing the directory directly",
            );
            remove_abandoned(dir);
        }
    }

    // Capture a passed construct-lane run's candidate (ADR-0152): commit the run
    // worktree's changes through the runner seam, then record both produced backend
    // objects as correspondence rows under their content-derived digests. Every
    // shortfall — a shell fault, a clean worktree (contradicting the passed
    // substantive-conclusion gate), a store write fault — folds to `None` with a
    // warn; the caller downgrades the verdict, so a lost capture reads as a
    // failed attempt, never a pass whose work silently evaporated.
    fn capture_candidate(&self, worktree_dir: &Path, nonce: &Nonce, message: Option<&str>) -> Option<CandidateRef> {
        let captured = match self.runner.capture(worktree_dir, message) {
            Ok(Some(captured)) => captured,
            Ok(None) => {
                tracing::warn!(
                    nonce = %nonce.0,
                    "local executor backend: passed run left a clean worktree — nothing to capture, failing closed",
                );
                return None;
            }
            Err(error) => {
                tracing::warn!(nonce = %nonce.0, %error, "local executor backend: candidate capture failed");
                return None;
            }
        };
        let candidate = CandidateRef {
            tree: candidate_tree_digest(&captured.tree),
            checkout: capture_commit_digest(&captured.commit),
        };
        match self
            .correspondence
            .record(&candidate.tree, &captured.tree)
            .and_then(|()| self.correspondence.record(&candidate.checkout, &captured.commit))
        {
            Ok(()) => {
                self.file_capture_diff(nonce, captured.diff.as_deref());
                Some(candidate)
            }
            Err(error) => {
                tracing::warn!(nonce = %nonce.0, %error, "local executor backend: candidate correspondence write failed");
                None
            }
        }
    }

    // File the lap's own diff against the nonce of the order that produced it
    // (#4959), so the intake broker can triage a passing repair lap against the
    // finding it was dispatched for before the re-judge is bought.
    //
    // Keyed by nonce rather than re-keyed to the member the way the commit
    // message is: the diff belongs to *this lap*, not to the member's current
    // candidate, and the reader is the admission of this lap's own order.
    //
    // Best-effort throughout, and capped: a diff the triage would refuse to read
    // anyway is not worth a row. A miss leaves the lap untriaged, which is the
    // pass side of an advisory-strict check.
    fn file_capture_diff(&self, nonce: &Nonce, diff: Option<&str>) {
        let (Some(store), Some(diff)) = (self.messages.as_ref(), diff) else {
            return;
        };
        if diff.is_empty() || diff.len() > MAX_TRIAGED_DIFF_BYTES {
            return;
        }
        // The guard is dropped with the statement, before the report below.
        let filed = store.lock().unwrap_or_else(PoisonError::into_inner).record_capture_diff(&nonce.0, diff);
        if let Err(error) = filed {
            tracing::warn!(
                nonce = %nonce.0,
                %error,
                "local executor backend: capture diff write failed; this lap's repair will not be triaged",
            );
        }
    }

    // File a captured candidate's commit message against the member its order
    // names, re-keying from the nonce the backend knows to the (bloom, workpiece)
    // pair the land path reads — the same key the review findings channel uses.
    //
    // Best-effort throughout: no mounted store, an order that has already gone,
    // or a write fault each cost the landing proposal its authored title and
    // nothing more, so none of them may downgrade a candidate that was captured
    // successfully.
    fn file_commit_message(&self, nonce: &Nonce, message: &str) {
        let Some(messages) = self.messages.as_ref() else {
            return;
        };
        // The lock is held for the read-then-write pair and dropped before the
        // report below: the re-key has to see the order it read.
        let filed = {
            let mut store = messages.lock().unwrap_or_else(PoisonError::into_inner);
            store.lookup_order(&nonce.0).and_then(|order| {
                order.map_or(Ok(false), |order| {
                    store.record_candidate_commit_message(&order.bloom, &order.workpiece, message).map(|()| true)
                })
            })
        };
        match filed {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                nonce = %nonce.0,
                "local executor backend: no outstanding order to key the captured candidate's commit message by",
            ),
            Err(error) => tracing::warn!(
                nonce = %nonce.0,
                %error,
                "local executor backend: candidate commit-message write failed; the landing proposal will fall back",
            ),
        }
    }
}

/// The content-derived digest of a captured candidate tree: a domain-tagged
/// address over the backend tree object's raw bytes, so the digest changes
/// exactly when the captured content does — ADR-0152's supersession property
/// falls out of the identity choice.
#[derive(Serialize)]
struct CandidateTreeAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for CandidateTreeAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.candidate.tree";
}

fn candidate_tree_digest(tree: &BackendObjectId) -> Digest {
    digest_of(&CandidateTreeAddress { object: tree.as_bytes() })
}

/// The content-derived digest of a capture commit — the [`CandidateRef::checkout`]
/// axis, distinct from the tree's by domain tag so the two never collide even
/// over equal object bytes.
#[derive(Serialize)]
struct CaptureCommitAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for CaptureCommitAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.candidate.checkout";
}

fn capture_commit_digest(commit: &BackendObjectId) -> Digest {
    digest_of(&CaptureCommitAddress { object: commit.as_bytes() })
}

/// Fail-closed evidence for an exited run that left no readable file — the
/// attempt still has to feed retry/wedge rather than loop on a missing path.
fn synthesized_missing_evidence(handle: &WorkHandle, subject: &Digest) -> Vec<EvidenceRef> {
    vec![EvidenceRef {
        name: NameEvidenceClaims::attempt_artifact_name(
            &handle.nonce,
            subject,
            StageVerdict::VerificationFailed,
            VerifyFailureSet::EMPTY,
            &Digest::of_wire_bytes(&[]),
        ),
        nonce: handle.nonce.clone(),
        artifact_id: 0,
        size_bytes: 0,
        candidate: None,
        findings: None,
        failed_verifiers: VerifyFailureSet::EMPTY,
        // Synthesized, not reported: there are no evidence bytes to read a
        // cost out of, so the attempt is unmeasured.
        cost: None,
        calls: None,
    }]
}

/// Render a resolved backend object as the lowercase hex sha the `git` argv
/// takes. The only place this backend spells a backend object in Git's own
/// notation — the correspondence, the digests, and the runner seam all carry
/// opaque bytes, and the rendering exists solely because the subprocess boundary
/// below is text.
fn render_object_hex(object: &BackendObjectId) -> String {
    let bytes = object.as_bytes();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

impl ExecutorBackend for LocalExecutor {
    type Error = LocalExecutorError;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        let pending = self.prepare(order)?;
        // Under the ceiling with nothing already waiting, the spawn happens inline,
        // so a spawn fault stays the caller's to re-drive exactly as it was before
        // the ceiling existed. Otherwise the dispatch waits its turn — and is acked
        // as submitted either way, so the reducer's view of it never depends on how
        // busy this host happened to be.
        if let Some(slot) = self.reserve_slot() {
            self.start_reserved(pending, slot)?;
        } else {
            self.enqueue(pending);
        }
        Ok(WorkHandle::new(order.nonce.clone()))
    }

    // `run` is a `&mut` reborrow from the registry guard (poll mutates the child),
    // so the guard must outlive it; the lint's "merge into a single expression" fix
    // would drop the guard before the reborrow, so it is suppressed here.
    #[allow(clippy::significant_drop_tightening, reason = "run is a &mut reborrow; the guard must outlive it")]
    fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        // Read the lifecycle out of the guarded region and drop the lock before
        // returning — the guard need only be held for the poll.
        let lifecycle = {
            let mut registry = self.lock();
            let Some(run) = registry.runs.get_mut(&handle.nonce.0) else {
                // A dispatch still waiting for a lane slot is dispatched but not
                // started — exactly what `Queued` names. Reporting it keeps a
                // throttled lane legible in the reactor's pending log instead of
                // reading as a run this backend lost track of.
                if registry.waiting.iter().any(|pending| pending.nonce == handle.nonce.0) {
                    return Ok(ExecutionStatus::Queued);
                }
                // Not tracked here is the clean Unknown, never an error — the same
                // "dispatch async, not visible yet" state the Actions backend reports.
                // A cancelled run has been evicted, so it also reports Unknown here
                // rather than reading the killed child's exit as a plain completion.
                return Ok(ExecutionStatus::Unknown);
            };
            run.process.poll()
        };
        Ok(match lifecycle {
            RunLifecycle::Running => ExecutionStatus::Running,
            RunLifecycle::Exited { success: true } => ExecutionStatus::Completed { conclusion: Conclusion::Success },
            RunLifecycle::Exited { success: false } => ExecutionStatus::Completed { conclusion: Conclusion::Failure },
        })
    }

    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error> {
        // Kill and evict under the lock. A failed kill returns early, leaving both
        // the entry and its slot claim in place.
        {
            let mut registry = self.lock();
            let Some(run) = registry.runs.get_mut(&handle.nonce.0) else {
                // A dispatch still waiting for a lane slot has no child to kill and
                // no checkout to reclaim, so dropping it from the queue is the whole
                // cancel — and it has to happen here, or the ceiling would go on to
                // spend a slot starting an order that has already expired.
                if let Some(index) = registry.waiting.iter().position(|pending| pending.nonce == handle.nonce.0) {
                    registry.waiting.remove(index);
                    tracing::info!(
                        nonce = %handle.nonce.0,
                        "local executor backend: cancel dropped a dispatch that was still waiting for a lane slot",
                    );
                    return Ok(());
                }
                // Idempotent (ADR-0177): no tracked run means the order was
                // never submitted to this backend or a prior cancel already
                // evicted it, and both are the "already absent" success the port
                // contract names. The deadline enforcement reissues its cancel
                // until the expired order is admitted, so refusing here would
                // make one store fault permanent.
                //
                // Absent here does not prove reclaimed, so say which it was in
                // the log rather than only in the return value. Boot
                // reconciliation (issue #4847) re-adopts a pre-restart order that
                // left a footprint under the scratch root, so reaching this arm
                // now means the order left none — nothing local to reclaim — or
                // this process already tore its run down. The nonce is what an
                // operator greps for if a checkout does turn out to be sitting
                // somewhere this reconciliation could not see.
                tracing::warn!(
                    nonce = %handle.nonce.0,
                    "local executor backend: cancel found no tracked run — nothing was killed or reclaimed here",
                );
                return Ok(());
            };
            run.process.kill()?;
            // A cancel is terminal — evict the killed run so the registry tracks only
            // in-flight orders rather than parking `cancelled` entries forever, and
            // hand its slot back. The slot's checkout stays where it is: the next
            // dispatch to hold the slot resets it, and removing it here would pull
            // the tree out from under whoever holds the slot by then.
            let slot = registry.runs.remove(&handle.nonce.0).and_then(|run| run.slot);
            registry.release_slot(slot);
        }
        // The eviction above freed a lane slot; hand it to whatever is waiting.
        self.pump();
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening, reason = "run is a &mut reborrow; the guard must outlive it")]
    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        // Pull the run's on-disk location, binding digest, and terminal exit out of
        // the guarded region, then drop the lock — the evidence read is blocking IO
        // and must not hold the registry mutex.
        let (evidence_dir, subject, lifecycle, LaneGates { is_construct, is_verify }, worktree_dir, reuse) = {
            let mut registry = self.lock();
            let Some(run) = registry.runs.get_mut(&handle.nonce.0) else {
                return Err(LocalExecutorError::NoRunForNonce(handle.nonce.clone()));
            };
            (
                run.evidence_dir.clone(),
                run.subject,
                run.process.poll(),
                run.gates,
                run.worktree_dir.clone(),
                run.reuse.clone(),
            )
        };
        let exited_success = matches!(lifecycle, RunLifecycle::Exited { success: true });
        let evidence_path = evidence_dir.join("evidence.json");
        let mut bytes = match fs::read(&evidence_path) {
            Ok(bytes) => bytes,
            // The run's own lifecycle is the terminal-vs-transient discriminator. An
            // Exited run that has left no readable evidence never will — re-driving
            // the read against it loops forever (the live 2026-07-18 bug), so this is
            // terminal: evict, free the slot, and synthesize a fail-closed
            // VerificationFailed attempt that feeds the retry/wedge machinery rather
            // than an error the intake re-drives. A Running run's missing file is
            // transient — keep the entry and its slot for the next cycle's retry.
            Err(read_error) => {
                if matches!(lifecycle, RunLifecycle::Exited { .. }) {
                    // Log the real IO fault before folding it into a fail-closed
                    // verdict — a permission/disk fault reads identically to a
                    // genuinely-absent evidence file once synthesized, so the fault
                    // must stay visible in the operator's logs (the same best-effort
                    // warn convention the reclaim path uses).
                    tracing::warn!(
                        nonce = %handle.nonce.0,
                        evidence = %evidence_path.display(),
                        error = %read_error,
                        "local executor backend: exited run left no readable evidence — failing closed",
                    );
                    self.retire(&handle.nonce.0);
                    // Terminal, so the lane slot is free — a run that failed without
                    // leaving evidence must release its slot exactly as a run that
                    // passed does, or a bloom whose lanes all fail this way wedges
                    // the queue behind them.
                    self.pump();
                    return Ok(synthesized_missing_evidence(handle, &subject));
                }
                return Err(LocalExecutorError::Evidence(format!("{}: {read_error}", evidence_path.display())));
            }
        };
        if let Some(plan) = reuse.as_ref() {
            bytes = self.record_session_reuse(&evidence_path, &bytes, plan, lifecycle);
        }
        // Evidence must identify the order that produced it before any body claim
        // is trusted. A stale or cross-wired evidence directory is otherwise able
        // to advance a different order merely by carrying a passing verdict.
        let nonce_matches = evidence_nonce_matches(&bytes, &handle.nonce);
        let failed_verifiers = if is_verify && nonce_matches {
            parse_failed_verifiers(&bytes)
        } else {
            Some(VerifyFailureSet::EMPTY)
        };
        // Verdict from the run's own evidence, lane-specific. The construct lane's
        // gate demands a substantive conclusion (#3596) — a terminal `result` with
        // `is_error == false` AND a produced candidate — and is fail-closed on any
        // shortfall (dead run, errored run, empty candidate, unparseable evidence),
        // so it never falls back to the child's terminal exit (an empty run exits
        // zero). The verify lane stamps a `status` ("pass"/"fail"); the raw
        // `exited_success` fallback survives only for a non-construct evidence shape
        // that stamps no status.
        let status = parse_status(&bytes);
        // An absent or unrecognized status still falls back to the child's
        // terminal exit; a recognized one is authoritative, and only `pass`
        // concludes.
        let status_passed = status.map_or(exited_success, |status| status == LaneStatus::Pass);
        let concluded = if is_construct {
            nonce_matches && construct_conclusion(&bytes)
        } else if is_verify {
            nonce_matches && failed_verifiers.is_some() && status_passed
        } else {
            nonce_matches && status_passed
        };
        // A passed construct-lane run's work is captured out of the slot checkout
        // it built in (ADR-0152) — commit + tree recorded as correspondence rows,
        // the digest pair riding the evidence reference — while that checkout
        // still holds it, which is until the next dispatch takes the slot.
        // Fail-closed: a passed run whose capture falls short downgrades to a
        // failing verdict rather than admitting a pass whose work was lost. A run
        // whose checkout this process cannot name (a boot re-adoption that
        // recovered no slot) captures nothing and takes the same downgrade.
        let commit_message = (is_construct && nonce_matches).then(|| parse_commit_message(&bytes)).flatten();
        let candidate = worktree_dir
            .filter(|_| is_construct && concluded)
            .and_then(|worktree_dir| self.capture_candidate(&worktree_dir, &handle.nonce, commit_message.as_deref()));
        // File the message against the member the run's order names, while that
        // order is still outstanding — the intake consumes it a moment later, and
        // the land path has no other way back from a bloom to the lane that wrote
        // this. Only for a candidate that was actually captured, so the row and
        // the candidate arrive together and a lane that produced nothing cannot
        // leave a message behind for the next one.
        if candidate.is_some()
            && let Some(message) = commit_message.as_deref()
        {
            self.file_commit_message(&handle.nonce, message);
        }
        let passed = concluded && (!is_construct || candidate.is_some());
        // The evidence has been consumed and any candidate captured — evict the
        // run so the registry tracks only in-flight orders rather than growing for
        // the process's lifetime, and hand its lane slot back. (The failed-read
        // path above returns early, keeping both the registry entry and the slot
        // claim for a later retry, so nothing resets the checkout the retry reads.)
        self.retire(&handle.nonce.0);
        // The run is off the registry, so its lane slot belongs to whatever has been
        // waiting for one.
        self.pump();
        // A lane that stamped `environment` claims it judged nothing (ADR-0176),
        // so it reports an executor fault rather than a failing verdict against
        // the subject. Gated on the nonce binding like every other body-derived
        // claim, and on the lane actually stamping a status — the construct lane
        // stamps none, and its gate is `construct_conclusion`, never this.
        let faulted = nonce_matches && !is_construct && status == Some(LaneStatus::Environment);
        let verdict = if passed {
            StageVerdict::VerificationPassed
        } else if faulted {
            StageVerdict::ExecutorFault
        } else {
            StageVerdict::VerificationFailed
        };
        // The detail digest is the content address of the evidence bytes — the
        // supporting artifact the verdict points at.
        let detail = Digest::of_wire_bytes(&bytes);
        let failed_verifiers = failed_verifiers.unwrap_or_default();
        let name =
            NameEvidenceClaims::attempt_artifact_name(&handle.nonce, &subject, verdict, failed_verifiers, &detail);
        Ok(vec![EvidenceRef {
            name,
            nonce: handle.nonce.clone(),
            // The local lane holds evidence on disk, not in a numbered artifact
            // store, so there is no backend artifact id; the name carries the whole
            // claim and the size is the file's length.
            artifact_id: 0,
            size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            candidate,
            findings: nonce_matches.then(|| parse_findings(&bytes)).flatten(),
            failed_verifiers,
            cost: nonce_matches.then(|| parse_cost(&bytes)).flatten(),
            calls: nonce_matches.then(|| parse_calls(&bytes)).flatten(),
        }])
    }
}

impl ReconcileLanes for LocalExecutor {
    /// Rebuild what this backend can know about local runs a previous process
    /// dispatched (issue #4847), by intersecting the store's live orders with the
    /// scratch checkouts still on disk.
    ///
    /// A live order that left a footprint under the scratch root comes back as a
    /// re-adopted run, so the port resolves it again instead of reporting
    /// `Unknown` forever and refusing every cancel with `NoRunForNonce`. A
    /// registered checkout with no live order is reclaimed, so it does not outlive
    /// the order that made it for the life of the host.
    ///
    /// Both halves read the *same* live set, which is what keeps the sweep safe:
    /// the only checkouts it removes are ones re-adoption already declined, so it
    /// can never pull one out from under an order still in flight.
    fn reconcile(&self, live: &[OutstandingDispatch]) -> ReconcileReport {
        let readopted =
            live.iter().filter(|dispatch| self.readopt(dispatch)).map(|dispatch| dispatch.nonce.clone()).collect();

        ReconcileReport {
            readopted,
            reclaimed: self.sweep_abandoned(&live.iter().map(|dispatch| dispatch.nonce.0.as_str()).collect()),
        }
    }

    fn lane_occupancy(&self) -> LaneOccupancy {
        let registry = self.lock();
        // `slots` is the allocator's own record of what is spoken for, so it
        // already covers a reservation that has not become a run: `reserve_slot`
        // claims the index before the spawn shells out.
        let slots = registry.slots.iter().copied().collect();
        // A re-adopted run whose evidence recorded no usable slot is building
        // somewhere this process cannot name.
        let unattributed = registry.runs.values().any(|run| run.slot.is_none());
        drop(registry);
        LaneOccupancy { slots, unattributed }
    }
}

/// Record which lane slot a dispatch is running in, beside that dispatch's own
/// evidence.
///
/// The slot decides the build path and the path is reused, so a coordinator that
/// restarts mid-dispatch has to be able to find the slot a surviving child is
/// building in — otherwise the next dispatch claims that slot and resets the
/// checkout out from under it. The record lives in the evidence directory
/// because that is per dispatch and is already where boot reconciliation looks.
///
/// Best-effort: a record that cannot be written costs a re-adopted run its
/// checkout (it captures nothing and fails closed), never the dispatch itself.
fn record_slot(evidence_dir: &Path, slot: usize) {
    if let Err(error) =
        fs::create_dir_all(evidence_dir).and_then(|()| fs::write(evidence_dir.join(SLOT_RECORD), slot.to_string()))
    {
        tracing::warn!(
            evidence = %evidence_dir.display(),
            %error,
            "local executor backend: could not record the dispatch's lane slot; a restart will not recover its checkout",
        );
    }
}

/// The lane slot a dispatch recorded in its evidence directory, or `None` when
/// it recorded none (a dispatch from before this layout, or one whose record
/// could not be written) or recorded text that is not a slot index.
fn recorded_slot(evidence_dir: &Path) -> Option<usize> {
    fs::read_to_string(evidence_dir.join(SLOT_RECORD)).ok()?.trim().parse().ok()
}

/// Whether a scratch-root directory name is a lane slot's canonical checkout.
///
/// Exact rather than prefix-loose: the sweep reads this to decide what it must
/// *not* reclaim, and a nonce that merely began with the prefix would otherwise
/// leave an abandoned checkout on disk forever.
fn is_slot_directory(name: &str) -> bool {
    name.strip_prefix(SLOT_PREFIX)
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Where the per-slot target directories may be created: `configured` when a
/// deployment named a usable root, `scratch_root` otherwise (#4912).
///
/// An empty value is nothing named at all: the target dirs become siblings of
/// the slot checkouts, which needs no configuration and is on the volume the
/// checkouts already fit on.
///
/// A value inside a slot checkout is refused — in favour of that same default
/// rather than of a failed boot, since this is a cache location and no placement
/// of it is worth refusing to run blooms over. It is the one placement that
/// would silently undo the whole arrangement: every dispatch resets its slot with `git clean --force
/// --force -d -x`, which removes ignored files, so a target directory under a
/// checkout is deleted once per dispatch: the lane still builds, the counters
/// still read, and every lap is cold — the warm-dependency property is gone with
/// nothing saying so. Refusing it costs a deployment its chosen volume and says
/// why; honouring it costs every lap a full rebuild and says nothing.
fn usable_target_base(configured: &str, scratch_root: &Path) -> PathBuf {
    let configured = Path::new(configured);
    if configured.as_os_str().is_empty() {
        return scratch_root.to_path_buf();
    }
    if inside_a_slot_checkout(configured, scratch_root) {
        tracing::warn!(
            configured = %configured.display(),
            scratch_root = %scratch_root.display(),
            "local executor backend: the configured lane target base sits inside a slot checkout, where each \
             dispatch's `git clean` would delete it; building beside the checkouts instead",
        );
        return scratch_root.to_path_buf();
    }
    configured.to_path_buf()
}

/// Whether `path` lies inside one of `scratch_root`'s slot checkouts — the
/// placement [`usable_target_base`] refuses.
///
/// Read off the path rather than the filesystem: the checkouts a slot will use
/// are named by construction (`slot-<index>` under the root, see
/// [`LocalExecutor::slot_dir`]) and most of them do not exist yet at boot, so a
/// check that asked the disk would pass and then be wrong on the first dispatch.
/// Both sides are resolved against the cwd first, since the scratch root ships
/// relative and a deployment states its target volume absolute — comparing the
/// two as written would find no prefix and let the placement through.
fn inside_a_slot_checkout(path: &Path, scratch_root: &Path) -> bool {
    let resolved = absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let root = absolute(scratch_root).unwrap_or_else(|_| scratch_root.to_path_buf());
    resolved
        .strip_prefix(&root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(is_slot_directory)
}

/// The dispatch nonce a registered worktree belongs to, or `None` when the
/// worktree is not one of this backend's.
///
/// Every checkout it makes is a direct child of the (canonical) scratch root, so
/// anything else in the repo's worktree list — the operator's own, another
/// tool's, one under a different root — is outside the sweep's remit and reads
/// as `None`. A slot checkout is a direct child too and reads as its own name,
/// which the sweep recognizes through [`is_slot_directory`] rather than as a
/// nonce nobody is waiting on.
fn scratch_nonce_of(base: &Path, worktree: &Path) -> Option<String> {
    let parent = fs::canonicalize(worktree.parent()?).ok()?;
    if parent != *base {
        return None;
    }
    Some(worktree.file_name()?.to_str()?.to_owned())
}

/// Remove an abandoned scratch directory, folding a failure into a warn — the
/// caller is a best-effort cleanup, and a directory that will not go away must
/// not fail a boot. An already-absent path is the reclaim already being done,
/// not a failure.
fn remove_abandoned(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "local executor backend: abandoned scratch directory could not be removed",
        ),
    }
}

/// Whether `bytes` carry a top-level nonce that decodes as an executor handle
/// and names exactly `expected`. Evidence bodies are untrusted until this binds
/// them to the registry entry that supplied their directory.
fn evidence_nonce_matches(bytes: &[u8], expected: &Nonce) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let Some(nonce) = value.get("nonce").filter(|nonce| nonce.is_string()) else {
        return false;
    };
    serde_json::from_value::<Nonce>(nonce.clone()).is_ok_and(|actual| actual == *expected)
}

/// What a lane's stamped `status` claims. Three-valued rather than a boolean
/// because `environment` is not a verdict on the subject at all (ADR-0176): the
/// lane is reporting that it could not judge one, and collapsing that into
/// `Fail` is what charged a member repair lap for a host outage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LaneStatus {
    Pass,
    Fail,
    Environment,
}

/// Read a lane's `status` field from an `evidence.json` byte string. `None` when
/// the field is absent (the construct lane's record carries no status), carries
/// an unrecognized token, or the bytes are not a decodable object — the caller
/// falls back to the child's terminal exit, which is fail-closed.
fn parse_status(bytes: &[u8]) -> Option<LaneStatus> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match value.get("status").and_then(serde_json::Value::as_str)? {
        "pass" => Some(LaneStatus::Pass),
        "fail" => Some(LaneStatus::Fail),
        "environment" => Some(LaneStatus::Environment),
        _ => None,
    }
}

/// Decode the optional body-derived ADR-0178 failure set. Absence is the valid
/// empty/pass representation; a present malformed or noncanonical value is an
/// invalid body (`None`) and makes the local verdict fail closed.
fn parse_failed_verifiers(bytes: &[u8]) -> Option<VerifyFailureSet> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value
        .get("failed_verifiers")
        .map_or(Some(VerifyFailureSet::EMPTY), |failures| serde_json::from_value(failures.clone()).ok())
}

// The evidence's top-level `findings` prose — what the review critic stamped
// (#3656), threaded onto a later Refine re-entry. Presence-driven: a lane that
// stamps none yields `None`, no lane flag needed.
fn parse_findings(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value.get("findings").and_then(serde_json::Value::as_str).map(str::to_owned)
}

/// The evidence's top-level `commit_message` prose — what the construct/refine
/// lane's agent wrote for the change it just made. Presence-driven like
/// [`parse_findings`]: a lane that wrote none yields `None`, and so does a blank
/// one, because an empty message names neither a capture subject nor a title.
fn parse_commit_message(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value
        .get("commit_message")
        .and_then(serde_json::Value::as_str)
        .map(|message| message.trim().to_owned())
        .filter(|message| !message.is_empty())
}

/// What the attempt cost, from the `result_record` the lane nested in its
/// `evidence.json` (#4679) — the same object `construct_conclusion` reads
/// `is_error` out of, parsed for its token and price columns instead.
///
/// Presence-driven like [`parse_findings`], and `None` at every shortfall: a
/// lane that nests no record, bytes that do not decode, or a record whose
/// columns do not parse. `None` means *unmeasured* and writes no study row —
/// the alternative, a row of zeroes, would make an unmeasured attempt
/// indistinguishable from a free one and quietly corrupt every average taken
/// over the ledger.
fn parse_measured(bytes: &[u8]) -> Option<(StudyCost, Option<Vec<aether_bloomery::StudyCall>>)> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    parse_study(&serde_json::to_vec(value.get("result_record")?).ok()?).ok()
}

fn parse_cost(bytes: &[u8]) -> Option<StudyCost> {
    parse_measured(bytes).map(|(cost, _)| cost)
}

fn parse_calls(bytes: &[u8]) -> Option<Vec<aether_bloomery::StudyCall>> {
    parse_measured(bytes).and_then(|(_, calls)| calls)
}

/// Whether a construct lane's `evidence.json` byte string shows a **substantive
/// conclusion** (#3596): the run reached a terminal `result` with
/// `is_error == false` *and* left a candidate change in the working tree
/// (`produced_candidate == true`). The construct lane's whole job is to produce a
/// focused candidate change, so a run that merely exited zero with nothing to
/// review must not advance the member. Fail-closed — a `no_result` record (a run
/// that died early), an errored run (`is_error == true`), an empty candidate
/// (`produced_candidate` absent or `false`), or bytes that do not decode all
/// return `false`.
fn construct_conclusion(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let produced_candidate = value.get("produced_candidate").and_then(serde_json::Value::as_bool).unwrap_or(false);
    // A terminal `result` with is_error == false is the "the run concluded"
    // signal; a `no_result` record carries no `is_error` field, and an errored run
    // carries `is_error == true` — both fail this test.
    let concluded =
        value.get("result_record").and_then(|record| record.get("is_error")).and_then(serde_json::Value::as_bool)
            == Some(false);
    concluded && produced_candidate
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::usable_target_base;

    /// The scratch root these cases are stated against, absolute the way a
    /// production one is.
    fn scratch_root() -> PathBuf {
        PathBuf::from("/mnt/scratch/bloomery")
    }

    #[test]
    fn an_unconfigured_target_base_puts_the_build_trees_beside_the_checkouts() {
        assert_eq!(
            usable_target_base("", &scratch_root()),
            scratch_root(),
            "nothing named means the slot's target dir is its checkout's sibling, which needs no configuration",
        );
        assert_eq!(
            usable_target_base("/mnt/big/bloomery-targets", &scratch_root()),
            Path::new("/mnt/big/bloomery-targets"),
            "a deployment that named a roomier volume builds on it",
        );
    }

    #[test]
    fn a_target_base_inside_a_slot_checkout_is_refused() {
        // Tripwire (#4912): every dispatch resets its slot with `git clean
        // --force --force -d -x`, which removes ignored files — so a target
        // directory anywhere under a slot checkout is deleted once per
        // dispatch. Nothing reports that: the lane still builds, the sccache
        // counters still read, and every lap is simply cold again, which is
        // precisely the cost the slot layout exists to avoid. Honouring the
        // path is silent; refusing it is one warn line.
        for configured in ["/mnt/scratch/bloomery/slot-0/target", "/mnt/scratch/bloomery/slot-12/nested/target"] {
            assert_eq!(
                usable_target_base(configured, &scratch_root()),
                scratch_root(),
                "{configured} would be wiped by its own slot's checkout hygiene",
            );
        }

        // The complement: a slot's own target directory is a *sibling* of the
        // checkout, so a path merely starting with a slot's name must not be
        // read as being inside one — that reading would refuse the default
        // arrangement itself.
        assert_eq!(
            usable_target_base("/mnt/scratch/bloomery/slot-0-target", &scratch_root()),
            Path::new("/mnt/scratch/bloomery/slot-0-target"),
            "a slot's own target directory is its checkout's sibling, not a path inside it",
        );
    }
}
