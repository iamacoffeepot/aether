//! The local-process executor backend: an in-process registry of tracked runs
//! over the [`TransformRunner`] spawn seam, and its [`ExecutorBackend`] impl.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf, absolute};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_bloomery::digest::ContentAddressed;
use aether_bloomery::{
    BackendObjectId, CandidateRef, Conclusion, Digest, EvidenceRef, ExecutionStatus, ExecutorBackend, Nonce,
    ResolvedModel, SharedCorrespondence, StageVerdict, StudyCost, Transformation, VerifyFailureSet, WorkHandle,
    WorkOrder, digest_of, is_model_lane,
};
use aether_bloomery_github::parse_study_cost;
use serde::Serialize;
use std::fs;

use super::error::LocalExecutorError;
use super::lane_program::LaneProgram;
use super::orphan::OrphanedRun;
use super::process_runner::{CaptureIdentity, ProcessTransformRunner};
use super::runner::{RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::bloomery::CONSTRUCT_IMPLEMENT_COMMAND;
use crate::bloomery::CoordinatorConfig;
use crate::bloomery::executor::{OutstandingDispatch, ReconcileLanes, ReconcileReport};
use crate::bloomery::intake::NameEvidenceClaims;
use crate::store::{SqliteStore, StoreBackend};

/// The suffix distinguishing a run's evidence directory from its scratch
/// worktree under the same nonce-keyed base dir.
const EVIDENCE_SUFFIX: &str = "-evidence";

/// One tracked run: the spawned child, its scratch worktree, where its evidence
/// lands, and the digest the returning evidence must bind to.
struct Run {
    process: Box<dyn RunProcess>,
    // The scratch worktree `start` materialized the checkout into, released on the
    // run's terminal path (cancel, or evidence consumed) so a long-lived backend
    // does not leak one `git worktree` per order.
    worktree_dir: PathBuf,
    evidence_dir: PathBuf,
    // The digest the intake broker binds the evidence to, per `evidence_subject`.
    subject: Digest,
    // Which lane-specific evidence gates this run's verdict rides, decided from
    // the order's command.
    gates: LaneGates,
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
    worktree_dir: PathBuf,
    evidence_dir: PathBuf,
    profile: Option<ResolvedModel>,
    task: Option<String>,
    subject: Digest,
    gates: LaneGates,
}

impl PendingRun {
    fn spec(&self) -> RunSpec<'_> {
        RunSpec {
            command: &self.command,
            checkout_hex: &self.checkout_hex,
            diff_base_hex: self.diff_base_hex.as_deref(),
            worktree_dir: &self.worktree_dir,
            evidence_dir: &self.evidence_dir,
            nonce: &self.nonce,
            harness: self.profile.as_ref().map(|resolved| resolved.harness.as_str()),
            model: self.profile.as_ref().map(|resolved| resolved.model.as_str()),
            effort: self.profile.as_ref().map(|resolved| resolved.effort.as_str()),
            task: self.task.as_deref(),
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
}

impl Registry {
    // How many lane slots are spoken for: the spawned runs, plus the starts that
    // have not yet become one.
    fn occupied(&self) -> usize {
        self.runs.len() + self.starting
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
    // The store the captured candidate's commit message is filed in, keyed by the
    // member the run's order names. `None` for a backend built without one (the
    // seam tests), which simply files nothing.
    //
    // Its own connection to the coordinator's store file, exactly as
    // `SqliteCorrespondence` and the dispatch reactor open theirs — the WAL
    // journal serializes the rare concurrent write. Behind a `Mutex` because the
    // port's methods take `&self` while the store writes through `&mut`.
    messages: Option<Mutex<SqliteStore>>,
}

impl LocalExecutor {
    /// Build a backend over an explicit spawn seam — the seam tests drive with a
    /// stub runner, and [`from_config`](Self::from_config) drives with the
    /// production [`ProcessTransformRunner`]. `base_dir` is the scratch-worktree
    /// root; each run gets `base_dir/<nonce>` (worktree) and
    /// `base_dir/<nonce>-evidence` (output). Unthrottled — every submit spawns
    /// immediately until [`with_max_concurrent_lanes`](Self::with_max_concurrent_lanes)
    /// sets a ceiling, which is what [`from_config`](Self::from_config) does.
    #[must_use]
    pub fn new(
        runner: Arc<dyn TransformRunner>,
        correspondence: SharedCorrespondence,
        base_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            runner,
            correspondence,
            base_dir: base_dir.into(),
            registry: Mutex::new(Registry::default()),
            max_concurrent_lanes: usize::MAX,
            messages: None,
        }
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

    /// Build the production backend from resolved config: the real git + cargo
    /// [`ProcessTransformRunner`], the shared `correspondence` the checkout
    /// resolves through, and the config'd scratch-worktree base dir. The model a
    /// run executes under is not config — it rides each order as the resolved
    /// agent profile the host overlaid at dispatch (ADR-0149 §The line).
    #[must_use]
    pub fn from_config(config: &CoordinatorConfig, correspondence: SharedCorrespondence) -> Self {
        let identity = CaptureIdentity { name: config.operator_name.clone(), email: config.operator_email.clone() };

        let backend = Self::new(
            Arc::new(ProcessTransformRunner::new(identity, LaneProgram::parse(&config.local_lane_program))),
            correspondence,
            config.local_worktree_base.clone(),
        )
        .with_max_concurrent_lanes(config.max_concurrent_lanes);
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

    // The two directories a run at `nonce` owns: its scratch worktree and its
    // evidence output dir.
    //
    // Resolved absolute against the coordinator's own cwd. The child runs with
    // `current_dir(worktree_dir)`, so a relative `--out` (the config default
    // `local_worktree_base` ships relative) would resolve against the *child's*
    // cwd — the scratch worktree — while `stream_evidence` reads `evidence_dir`
    // against the *coordinator's* cwd; the two diverge and the intake polls a path
    // the run never wrote, forever. `std::path::absolute` is a lexical cwd-join
    // that does not require the path to exist (unlike `canonicalize`).
    //
    // The single spelling of the nonce→path convention, because the boot
    // reconciliation reads it backwards: it recovers a nonce from a directory name
    // under `base_dir`, which only works while both sides agree on the layout.
    fn run_paths(&self, nonce: &str) -> io::Result<(PathBuf, PathBuf)> {
        Ok((absolute(self.base_dir.join(nonce))?, absolute(self.base_dir.join(format!("{nonce}{EVIDENCE_SUFFIX}")))?))
    }

    // Release a terminal run's scratch worktree off the registry lock (the teardown
    // is a blocking git shell-out), folding a failure into a warn rather than the
    // terminal op's result — the child is already dead / the evidence already read,
    // so a cleanup miss must not fail the cancel or the evidence stream.
    fn release_worktree(&self, worktree_dir: &Path) {
        if let Err(error) = self.runner.release(worktree_dir) {
            tracing::warn!(
                worktree = %worktree_dir.display(),
                %error,
                "local executor backend: scratch worktree release failed",
            );
        }
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
        let (worktree_dir, evidence_dir) = self.run_paths(&nonce).map_err(LocalExecutorError::Io)?;
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
            worktree_dir,
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
    fn reserve_slot(&self) -> bool {
        let mut registry = self.lock();
        if !registry.waiting.is_empty() || registry.occupied() >= self.max_concurrent_lanes {
            return false;
        }
        registry.starting += 1;
        true
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

    // Spawn a dispatch holding a reserved slot, turning the reservation into a
    // tracked run. The spawn itself runs off the registry lock — it shells out to
    // git — and the reservation is what keeps the slot it is spending from being
    // handed to anyone else meanwhile. Releases the reservation on both paths.
    fn start_reserved(&self, pending: PendingRun) -> Result<(), LocalExecutorError> {
        let started = self.runner.start(&pending.spec());

        {
            let mut registry = self.lock();
            registry.starting -= 1;
            registry.runs.insert(
                pending.nonce,
                Run {
                    process: started?,
                    worktree_dir: pending.worktree_dir,
                    evidence_dir: pending.evidence_dir,
                    subject: pending.subject,
                    gates: pending.gates,
                },
            );
        }
        Ok(())
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
        while let Some(pending) = self.take_waiting() {
            let nonce = pending.nonce.clone();
            if let Err(error) = self.start_reserved(pending) {
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
    fn take_waiting(&self) -> Option<PendingRun> {
        let mut registry = self.lock();
        if registry.occupied() >= self.max_concurrent_lanes {
            return None;
        }
        // Reserved as it is taken: the slot is spent from here, not from whenever
        // the spawn it is handed to returns.
        registry.waiting.pop_front().inspect(|_| registry.starting += 1)
    }

    // Re-adopt one live order's run, if the previous process left a local footprint
    // for it. Returns whether it was re-adopted.
    //
    // Either directory counts as that footprint. The evidence dir is created first
    // (before the checkout, before the spawn), so a dispatch that reached the local
    // lane at all has one; the worktree may already have been released while the
    // order stayed outstanding. Re-adopting on the evidence dir alone still
    // recovers the run's verdict, which is the part the order is waiting on.
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
        let (worktree_dir, evidence_dir) = match self.run_paths(&nonce.0) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(
                    nonce = %nonce.0,
                    %error,
                    "local executor backend: could not resolve a re-adopted run's paths",
                );
                return false;
            }
        };
        if !worktree_dir.exists() && !evidence_dir.exists() {
            return false;
        }
        let mut registry = self.lock();
        if registry.runs.contains_key(&nonce.0) {
            return false;
        }
        registry.runs.insert(
            nonce.0.clone(),
            Run {
                process: Box::new(OrphanedRun::new(nonce.clone(), &evidence_dir)),
                worktree_dir,
                evidence_dir,
                subject: evidence_subject(&dispatch.transformation),
                gates: LaneGates::of(&dispatch.transformation.command),
            },
        );
        true
    }

    // Reclaim the scratch checkouts belonging to no live order, returning how many
    // were reclaimed.
    //
    // The candidates are the repo's *registered* worktrees, not the scratch root's
    // directory listing. The listing cannot tell this backend's checkouts from
    // anything else a deployment keeps under the configured root — and it is a
    // configured root, so the sweep must not assume it owns everything below it;
    // acting on a directory listing means deleting an operator's files on the
    // strength of where they sat. A registration, filtered to direct children of
    // the root, is positive proof of a checkout this backend created: `git
    // worktree add` at `base_dir/<nonce>` is the only thing that makes one.
    //
    // What that leaves behind is a dispatch that died between creating its
    // directory and registering the worktree. That is bounded litter at a nonce
    // path no dispatch reuses, and `reclaim_worktree_path` clears it if the same
    // nonce ever dispatches again — a fair trade against a sweep that could delete
    // something it does not own.
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
            if live.contains(nonce.as_str()) {
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
            Ok(()) => Some(candidate),
            Err(error) => {
                tracing::warn!(nonce = %nonce.0, %error, "local executor backend: candidate correspondence write failed");
                None
            }
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
        if self.reserve_slot() {
            self.start_reserved(pending)?;
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
        // Kill and evict under the lock, then pull the run's worktree out so the
        // teardown runs off the lock. A failed kill returns early, leaving both the
        // entry and the worktree in place.
        let worktree_dir = {
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
            let worktree_dir = run.worktree_dir.clone();
            // A cancel is terminal — evict the killed run so the registry tracks only
            // in-flight orders rather than parking `cancelled` entries forever.
            registry.runs.remove(&handle.nonce.0);
            worktree_dir
        };
        self.release_worktree(&worktree_dir);
        // The eviction above freed a lane slot; hand it to whatever is waiting.
        self.pump();
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening, reason = "run is a &mut reborrow; the guard must outlive it")]
    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        // Pull the run's on-disk location, binding digest, and terminal exit out of
        // the guarded region, then drop the lock — the evidence read is blocking IO
        // and must not hold the registry mutex.
        let (evidence_dir, subject, lifecycle, LaneGates { is_construct, is_verify }, worktree_dir) = {
            let mut registry = self.lock();
            let Some(run) = registry.runs.get_mut(&handle.nonce.0) else {
                return Err(LocalExecutorError::NoRunForNonce(handle.nonce.clone()));
            };
            (run.evidence_dir.clone(), run.subject, run.process.poll(), run.gates, run.worktree_dir.clone())
        };
        let exited_success = matches!(lifecycle, RunLifecycle::Exited { success: true });
        let evidence_path = evidence_dir.join("evidence.json");
        let bytes = match fs::read(&evidence_path) {
            Ok(bytes) => bytes,
            // The run's own lifecycle is the terminal-vs-transient discriminator. An
            // Exited run that has left no readable evidence never will — re-driving
            // the read against it loops forever (the live 2026-07-18 bug), so this is
            // terminal: evict, release the worktree, and synthesize a fail-closed
            // VerificationFailed attempt that feeds the retry/wedge machinery rather
            // than an error the intake re-drives. A Running run's missing file is
            // transient — keep the entry and worktree for the next cycle's retry.
            Err(read_error) => {
                if matches!(lifecycle, RunLifecycle::Exited { .. }) {
                    // Log the real IO fault before folding it into a fail-closed
                    // verdict — a permission/disk fault reads identically to a
                    // genuinely-absent evidence file once synthesized, so the fault
                    // must stay visible in the operator's logs (the same best-effort
                    // warn convention `release_worktree` uses).
                    tracing::warn!(
                        nonce = %handle.nonce.0,
                        evidence = %evidence_path.display(),
                        error = %read_error,
                        "local executor backend: exited run left no readable evidence — failing closed",
                    );
                    self.lock().runs.remove(&handle.nonce.0);
                    self.release_worktree(&worktree_dir);
                    // Terminal, so the lane slot is free — a run that failed without
                    // leaving evidence must release its slot exactly as a run that
                    // passed does, or a bloom whose lanes all fail this way wedges
                    // the queue behind them.
                    self.pump();
                    return Ok(vec![EvidenceRef {
                        name: NameEvidenceClaims::attempt_artifact_name(
                            &handle.nonce,
                            &subject,
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
                        // Synthesized, not reported: there are no evidence bytes
                        // to read a cost out of, so the attempt is unmeasured.
                        cost: None,
                    }]);
                }
                return Err(LocalExecutorError::Evidence(format!("{}: {read_error}", evidence_path.display())));
            }
        };
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
        // A passed construct-lane run's work is captured while its worktree still
        // exists (ADR-0152) — commit + tree recorded as correspondence rows, the
        // digest pair riding the evidence reference. Fail-closed: a passed run
        // whose capture falls short downgrades to a failing verdict rather than
        // admitting a pass whose work was lost with the worktree below.
        let commit_message = (is_construct && nonce_matches).then(|| parse_commit_message(&bytes)).flatten();
        let candidate = if is_construct && concluded {
            self.capture_candidate(&worktree_dir, &handle.nonce, commit_message.as_deref())
        } else {
            None
        };
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
        // the process's lifetime, and reclaim its scratch worktree so the checkout
        // does not outlive the run. (The failed-read path above returns early,
        // keeping both the registry entry and the worktree for a later retry.)
        self.lock().runs.remove(&handle.nonce.0);
        self.release_worktree(&worktree_dir);
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
}

/// The dispatch nonce a registered worktree belongs to, or `None` when the
/// worktree is not one of this backend's.
///
/// Every scratch checkout it makes is a direct child of the (canonical) scratch
/// root named for its dispatch nonce, so anything else in the repo's worktree
/// list — the operator's own, another tool's, one under a different root — is
/// outside the sweep's remit and reads as `None`.
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
fn parse_cost(bytes: &[u8]) -> Option<StudyCost> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    parse_study_cost(&serde_json::to_vec(value.get("result_record")?).ok()?).ok()
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
