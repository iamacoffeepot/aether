//! The batch gate (ADR-0200 §"The batch gate"): disjoint-surface members
//! compose into one verification checkout, one build proves everyone, and
//! a failure walks the attribution ladder back to the member that owns it.
//!
//! Batch membership is an execution detail. A member is done when its facts
//! are green; it neither waits for nor answers for batchmates beyond sharing
//! the build. A charged failure returns as [`MemberFate::Resume`] so the
//! executor can resume that member's lane session in place (#4986).

use std::mem::take;

use aether_bloomery::{WorkpieceId, surface_intersection};

use super::{
    Attribution, AttributionError, AttributionRequest, BaseProbe, ClosureKey, DiscriminatedFacts, HostClass,
    ProofSource, RepairBoard, TaintSet, attribute_gate_failure, record_proof_facts,
};
use crate::store::StoreBackend;

/// Observed defaults for the adaptive restart knobs: a running gate is
/// young for one minute, and twenty-four newly-resolved members is a large
/// addition (the eight-finish-then-twenty-four-more case).
pub const DEFAULT_BATCH_RESTART_YOUNG_SECS: u64 = 60;
pub const DEFAULT_BATCH_RESTART_ADDITION: usize = 24;

/// Adaptive restart knobs (ADR-0200 §The batch gate).
///
/// A running gate restarts when waiting work exists *and* either the build
/// is still young or the arrival is large. A mature build is not preempted
/// by a small addition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BatchRestart {
    /// Age below which a running gate is young enough to preempt.
    pub young_secs: u64,
    /// Waiting-member count that preempts even a mature gate.
    pub large_addition: usize,
}

impl Default for BatchRestart {
    fn default() -> Self {
        Self { young_secs: DEFAULT_BATCH_RESTART_YOUNG_SECS, large_addition: DEFAULT_BATCH_RESTART_ADDITION }
    }
}

/// What the composer should do with waiting work.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Accumulation {
    /// Nothing waiting, nothing running.
    Idle,
    /// Work exists and no gate is running — start.
    Start,
    /// A gate is running; leave it. Waiting members join the next take.
    Keep,
    /// Preempt the running gate and start over the union.
    Restart,
}

/// A gate that has already been started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RunningGate {
    /// Seconds since the running gate began.
    pub age_secs: u64,
}

/// Decide whether to start, keep, or restart given waiting work and an
/// optional in-flight gate.
#[must_use]
pub fn decide_accumulation(waiting: usize, running: Option<RunningGate>, restart: BatchRestart) -> Accumulation {
    match running {
        None if waiting > 0 => Accumulation::Start,
        None => Accumulation::Idle,
        Some(_) if waiting == 0 => Accumulation::Keep,
        Some(running) if running.age_secs < restart.young_secs || waiting >= restart.large_addition => {
            Accumulation::Restart
        }
        Some(_) => Accumulation::Keep,
    }
}

/// One resolved member the batch can compose.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BatchMember {
    /// The workpiece this member is.
    pub workpiece: WorkpieceId,
    /// The composed-tree closure this member's facts are stamped at.
    pub closure: ClosureKey,
    /// The base-tree closure the ledger ladder consults.
    pub base_closure: ClosureKey,
    /// The declared surface that made this member eligible for the batch.
    pub declared_surface: Vec<String>,
}

/// Why a member could not join the waiting set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SurfaceOverlap {
    /// The member that was offered.
    pub member: WorkpieceId,
    /// The waiting member whose surface intersects it.
    pub existing: WorkpieceId,
}

/// Waiting set of pairwise-disjoint members, plus the restart knobs.
#[derive(Clone, Debug)]
pub struct BatchComposer {
    waiting: Vec<BatchMember>,
    restart: BatchRestart,
}

impl BatchComposer {
    /// An empty composer using `restart`.
    #[must_use]
    pub fn new(restart: BatchRestart) -> Self {
        Self { waiting: Vec::new(), restart }
    }

    /// Offer a newly-resolved member. Joins when its surface is disjoint from
    /// every waiting peer; otherwise the caller must run it in another batch.
    ///
    /// # Errors
    /// The offered surface intersects a member already waiting.
    pub fn offer(&mut self, member: BatchMember) -> Result<(), SurfaceOverlap> {
        if let Some(existing) = self
            .waiting
            .iter()
            .find(|waiting| !surface_intersection(&waiting.declared_surface, &member.declared_surface).is_empty())
        {
            return Err(SurfaceOverlap { member: member.workpiece, existing: existing.workpiece.clone() });
        }
        self.waiting.push(member);
        Ok(())
    }

    /// Members waiting for the next take, in offer order.
    #[must_use]
    pub fn waiting(&self) -> &[BatchMember] {
        &self.waiting
    }

    /// Drain the waiting set as the next gate's membership.
    #[must_use]
    pub fn take(&mut self) -> Vec<BatchMember> {
        take(&mut self.waiting)
    }

    /// [`decide_accumulation`] over this composer's waiting set.
    #[must_use]
    pub fn decide(&self, running: Option<RunningGate>) -> Accumulation {
        decide_accumulation(self.waiting.len(), running, self.restart)
    }
}

/// A classified failure from one batch-gate execution.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BatchFailure {
    /// rustc (or a sibling) named a file. Disjoint surfaces make the owner a
    /// lookup.
    FileOwned {
        /// Repository-relative path the diagnostic named.
        path: String,
    },
    /// A named test failed. The ledger ladder (#5128) resolves it.
    ClosureOwned {
        /// The failing test id.
        test_id: String,
        /// The member whose closure the test belongs to.
        member: WorkpieceId,
    },
    /// Feature unification, trait coherence, wire-tail collisions — no file
    /// and no named test. Bisect the batch.
    Unowned,
}

/// What one batch-gate invocation produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GateOutcome {
    /// Discrimination accepted only greens.
    Green(DiscriminatedFacts),
    /// The run failed. `facts` are whatever discrimination still accepted.
    Failed {
        /// The classified failure.
        failure: BatchFailure,
        /// Greens (and agreed reds) the run still proved.
        facts: DiscriminatedFacts,
    },
}

/// One discriminated pair of suite runs over the composed checkout.
pub trait BatchGate {
    /// Prove `members` as one composed tree. Called once per
    /// [`run_batch_gate`]; bisect uses [`BatchBisect`].
    fn run(&mut self, members: &[BatchMember]) -> GateOutcome;
}

/// Single-target reruns used to isolate an unowned residue. Each call is
/// one subset of the original batch, never the full suite again.
pub trait BatchBisect {
    /// Whether `subset` still produces the unowned failure.
    fn fails(&mut self, subset: &[BatchMember]) -> bool;
}

/// What became of one member after the batch ran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemberFate {
    /// This member's facts are green — done. Batch membership is forgotten.
    Proven,
    /// Attributed to this member. Resume its lane session in place (#4986).
    Resume,
    /// The ledger said the failure predates the member. No refine lap.
    Predating,
    /// Shared the build; not charged and not yet proven.
    Pending,
}

/// Per-member fates from one [`run_batch_gate`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BatchReport {
    /// How many times the prove gate ran. Always one: bisect is a different
    /// seam.
    pub gate_executions: usize,
    /// Fate of each input member, in input order.
    pub fates: Vec<(WorkpieceId, MemberFate)>,
}

/// Inputs the prove-and-attribute path shares with the ledger ladder.
pub struct BatchContext<'a> {
    /// The proof-fact ledger.
    pub store: &'a mut dyn StoreBackend,
    /// Host class facts are keyed on.
    pub host_class: &'a HostClass,
    /// Dispatch stamped on any fact this run records.
    pub producing_dispatch: &'a str,
    /// Bloom stamped on any fact this run records.
    pub producing_bloom: &'a [u8],
    /// Base commit the ledger probe and a base-repair body name.
    pub base_commit: &'a str,
}

/// The three rungs below file-owned lookup.
pub struct BatchFailureHooks<'a> {
    /// Base probe for the ledger ladder.
    pub probe: &'a mut dyn BaseProbe,
    /// Board that files a predating-red repair.
    pub board: &'a mut dyn RepairBoard,
    /// Culprit finder for unowned residue.
    pub bisect: &'a mut dyn BatchBisect,
}

/// Run one batch gate over `members` and attribute a failure if it produced
/// one. The gate is invoked exactly once; facts from that run are stamped on
/// every member's closure key.
///
/// # Errors
/// The ledger could not be read or written, or the board refused a repair.
pub fn run_batch_gate(
    ctx: &mut BatchContext<'_>,
    members: &[BatchMember],
    gate: &mut dyn BatchGate,
    hooks: &mut BatchFailureHooks<'_>,
) -> Result<BatchReport, AttributionError> {
    if members.is_empty() {
        return Ok(BatchReport { gate_executions: 0, fates: Vec::new() });
    }
    let outcome = gate.run(members);
    match outcome {
        GateOutcome::Green(facts) => {
            record_all(ctx, members, &facts)?;
            Ok(BatchReport { gate_executions: 1, fates: all_fates(members, MemberFate::Proven) })
        }
        GateOutcome::Failed { failure, facts } => {
            record_all(ctx, members, &facts)?;
            let fates = attribute_failure(ctx, members, &failure, hooks)?;
            Ok(BatchReport { gate_executions: 1, fates })
        }
    }
}

fn record_all(
    ctx: &mut BatchContext<'_>,
    members: &[BatchMember],
    facts: &DiscriminatedFacts,
) -> Result<(), AttributionError> {
    if facts.is_empty() {
        return Ok(());
    }
    let closures: Vec<ClosureKey> = members.iter().map(|member| member.closure).collect();
    record_proof_facts(
        ctx.store,
        &ProofSource::Aggregate { closures: &closures },
        facts,
        ctx.host_class,
        ctx.producing_dispatch,
        ctx.producing_bloom,
    )?;
    Ok(())
}

fn attribute_failure(
    ctx: &mut BatchContext<'_>,
    members: &[BatchMember],
    failure: &BatchFailure,
    hooks: &mut BatchFailureHooks<'_>,
) -> Result<Vec<(WorkpieceId, MemberFate)>, AttributionError> {
    let charged = match failure {
        BatchFailure::FileOwned { path } => owner_of_path(members, path).map(|member| (member, MemberFate::Resume)),
        BatchFailure::ClosureOwned { test_id, member } => ledger_rung(ctx, members, test_id, member, hooks)?,
        BatchFailure::Unowned => isolate_unowned(members, hooks.bisect).map(|member| (member, MemberFate::Resume)),
    };
    Ok(apply_charge(members, charged))
}

fn ledger_rung<'a>(
    ctx: &mut BatchContext<'_>,
    members: &'a [BatchMember],
    test_id: &str,
    member: &WorkpieceId,
    hooks: &mut BatchFailureHooks<'_>,
) -> Result<Option<(&'a BatchMember, MemberFate)>, AttributionError> {
    let Some(owned) = members.iter().find(|candidate| candidate.workpiece == *member) else {
        return Ok(None);
    };
    let request = AttributionRequest {
        base_closure: owned.base_closure,
        test_id,
        host_class: ctx.host_class,
        base_commit: ctx.base_commit,
        producing_dispatch: ctx.producing_dispatch,
        producing_bloom: ctx.producing_bloom,
    };
    let attribution = attribute_gate_failure(ctx.store, &request, hooks.probe, hooks.board, &TaintSet::new())?;
    let fate = match attribution {
        Attribution::Member => MemberFate::Resume,
        Attribution::Predating => MemberFate::Predating,
    };
    Ok(Some((owned, fate)))
}

fn isolate_unowned<'a>(members: &'a [BatchMember], bisect: &mut dyn BatchBisect) -> Option<&'a BatchMember> {
    if members.len() <= 1 {
        return members.first();
    }
    let mut lo = 0;
    let mut hi = members.len();
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if bisect.fails(&members[lo..mid]) {
            hi = mid;
        } else if bisect.fails(&members[mid..hi]) {
            lo = mid;
        } else {
            return None;
        }
    }
    Some(&members[lo])
}

fn owner_of_path<'a>(members: &'a [BatchMember], path: &str) -> Option<&'a BatchMember> {
    members.iter().find(|member| super::path_in_surface(&member.declared_surface, path))
}

fn all_fates(members: &[BatchMember], fate: MemberFate) -> Vec<(WorkpieceId, MemberFate)> {
    members.iter().map(|member| (member.workpiece.clone(), fate)).collect()
}

fn apply_charge(
    members: &[BatchMember],
    charged: Option<(&BatchMember, MemberFate)>,
) -> Vec<(WorkpieceId, MemberFate)> {
    members
        .iter()
        .map(|member| {
            let fate = charged
                .as_ref()
                .filter(|(owned, _)| owned.workpiece == member.workpiece)
                .map_or(MemberFate::Pending, |(_, fate)| *fate);
            (member.workpiece.clone(), fate)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{Digest, WorkpieceId};

    use super::{
        Accumulation, BatchBisect, BatchComposer, BatchContext, BatchFailure, BatchFailureHooks, BatchGate,
        BatchMember, BatchRestart, GateOutcome, MemberFate, RunningGate, decide_accumulation, run_batch_gate,
    };
    use crate::bloomery::verify::{
        AttributionError, BaseProbe, BaseRepairWorkpiece, ClosureKey, DiscriminatedFacts, HostClass, ProofResult,
        ProofSource, RepairBoard, RunnerReport, discriminate, record_proof_facts,
    };
    use crate::store::{SqliteStore, StoreBackend};

    const BLOOM: [u8; 32] = [0xB0; 32];
    const BASE_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn a_two_member_disjoint_batch_emits_facts_for_both_keys_from_one_gate() {
        // One composed checkout, one prove. Stamping only one key, or running
        // the gate twice, is the N-builds cost this gate exists to close.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let host = HostClass::new("fleet");
        let alpha = member("wp-a", 0xA1, &["crates/alpha/**"]);
        let gamma = member("wp-c", 0xC1, &["crates/gamma/**"]);
        let members = [alpha.clone(), gamma.clone()];
        let mut gate = ScriptedGate { runs: 0, outcome: GateOutcome::Green(green_facts(["alpha::ok", "gamma::ok"])) };
        let mut probe = ScriptedProbe { runs: 0 };
        let mut board = RecordingBoard::default();
        let mut bisect = ScriptedBisect { hits: Vec::new() };
        let mut hooks = BatchFailureHooks { probe: &mut probe, board: &mut board, bisect: &mut bisect };

        let report =
            run_batch_gate(&mut ctx(&mut store, &host), &members, &mut gate, &mut hooks).expect("a green batch proves");

        assert_eq!(gate.runs, 1, "the prove gate runs once over the composition");
        assert_eq!(report.gate_executions, 1);
        assert_eq!(
            report.fates,
            [(wp("wp-a"), MemberFate::Proven), (wp("wp-c"), MemberFate::Proven)],
            "member done means its facts are green"
        );

        let rows = store.list_proof_facts().expect("the table reads");
        assert!(rows.iter().any(|row| row.closure_key == alpha.closure.as_bytes()), "alpha's key must hold a fact");
        assert!(rows.iter().any(|row| row.closure_key == gamma.closure.as_bytes()), "gamma's key must hold a fact");
        assert!(rows.iter().all(|row| row.result == "green"));
    }

    #[test]
    fn a_file_owned_failure_charges_exactly_the_owning_member() {
        // Disjoint surfaces make a rustc path a lookup. Charging the
        // batchmate would spend a refine lap on a member that did not
        // write the file.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let host = HostClass::new("fleet");
        let members = [member("wp-a", 0xA1, &["crates/alpha/**"]), member("wp-c", 0xC1, &["crates/gamma/**"])];
        let mut gate = ScriptedGate {
            runs: 0,
            outcome: GateOutcome::Failed {
                failure: BatchFailure::FileOwned { path: "crates/alpha/src/lib.rs".to_owned() },
                facts: green_facts([] as [&str; 0]),
            },
        };
        let mut probe = ScriptedProbe { runs: 0 };
        let mut board = RecordingBoard::default();
        let mut bisect = ScriptedBisect { hits: Vec::new() };
        let mut hooks = BatchFailureHooks { probe: &mut probe, board: &mut board, bisect: &mut bisect };

        let report = run_batch_gate(&mut ctx(&mut store, &host), &members, &mut gate, &mut hooks)
            .expect("file-owned attribution");

        assert_eq!(gate.runs, 1);
        assert_eq!(
            report.fates,
            [(wp("wp-a"), MemberFate::Resume), (wp("wp-c"), MemberFate::Pending)],
            "only the file owner resumes; the batchmate is not charged"
        );
    }

    #[test]
    fn accumulation_starts_when_work_exists_and_preempts_only_young_or_large() {
        // Eight finish and the gate starts. Twenty-four more moments later
        // restart the young build over thirty-two. A mature build is not
        // restarted for a single late arrival.
        let restart = BatchRestart::default();
        assert_eq!(decide_accumulation(8, None, restart), Accumulation::Start);
        assert_eq!(decide_accumulation(24, Some(RunningGate { age_secs: 1 }), restart), Accumulation::Restart);
        assert_eq!(decide_accumulation(1, Some(RunningGate { age_secs: 120 }), restart), Accumulation::Keep);
        assert_eq!(decide_accumulation(24, Some(RunningGate { age_secs: 120 }), restart), Accumulation::Restart);
        assert_eq!(decide_accumulation(0, None, restart), Accumulation::Idle);
    }

    #[test]
    fn an_overlapping_member_cannot_join_the_waiting_set() {
        // The batch is disjoint-surface only. Pairing an overlap would make
        // file-owned lookup ambiguous and turn the shared build into a
        // fold-time collision.
        let mut composer = BatchComposer::new(BatchRestart::default());
        composer.offer(member("wp-a", 0xA1, &["crates/alpha/**"])).expect("the first member joins");
        let error =
            composer.offer(member("wp-b", 0xB1, &["crates/alpha/src/lib.rs"])).expect_err("an overlap must not join");
        assert_eq!(error.member, wp("wp-b"));
        assert_eq!(error.existing, wp("wp-a"));
        assert_eq!(composer.waiting().len(), 1);
    }

    #[test]
    fn a_closure_owned_failure_consults_the_ledger_before_charging() {
        // The #5128 rung: a green fact at the base is certain. A batch
        // that skipped the ledger would probe or charge without that
        // certainty.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let host = HostClass::new("fleet");
        let owned = member("wp-a", 0xA1, &["crates/alpha/**"]);
        seed_green(&mut store, owned.base_closure);
        let members = [owned, member("wp-c", 0xC1, &["crates/gamma/**"])];
        let mut gate = ScriptedGate {
            runs: 0,
            outcome: GateOutcome::Failed {
                failure: BatchFailure::ClosureOwned { test_id: "alpha::broke".to_owned(), member: wp("wp-a") },
                facts: green_facts([] as [&str; 0]),
            },
        };
        let mut probe = ScriptedProbe { runs: 0 };
        let mut board = RecordingBoard::default();
        let mut bisect = ScriptedBisect { hits: Vec::new() };
        let mut hooks = BatchFailureHooks { probe: &mut probe, board: &mut board, bisect: &mut bisect };

        let report = run_batch_gate(&mut ctx(&mut store, &host), &members, &mut gate, &mut hooks).expect("ledger rung");

        assert_eq!(probe.runs, 0, "a green base fact issues no rerun");
        assert_eq!(report.fates, [(wp("wp-a"), MemberFate::Resume), (wp("wp-c"), MemberFate::Pending)]);
    }

    #[test]
    fn an_unowned_residue_bisects_to_the_owning_member() {
        // Feature unification has no file. Without the O(log n) rung the
        // batch would charge everyone or no one.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let host = HostClass::new("fleet");
        let members = [member("wp-a", 0xA1, &["crates/alpha/**"]), member("wp-c", 0xC1, &["crates/gamma/**"])];
        let mut gate = ScriptedGate {
            runs: 0,
            outcome: GateOutcome::Failed { failure: BatchFailure::Unowned, facts: green_facts([] as [&str; 0]) },
        };
        let mut probe = ScriptedProbe { runs: 0 };
        let mut board = RecordingBoard::default();
        let mut bisect = ScriptedBisect { hits: vec![wp("wp-c")] };
        let mut hooks = BatchFailureHooks { probe: &mut probe, board: &mut board, bisect: &mut bisect };

        let report =
            run_batch_gate(&mut ctx(&mut store, &host), &members, &mut gate, &mut hooks).expect("bisect isolates");

        assert_eq!(gate.runs, 1, "bisect does not re-run the prove gate");
        assert_eq!(report.fates, [(wp("wp-a"), MemberFate::Pending), (wp("wp-c"), MemberFate::Resume)]);
    }

    fn member(name: &str, fill: u8, surface: &[&str]) -> BatchMember {
        let closure = ClosureKey::from_digest(Digest::from_bytes([fill; 32]));
        BatchMember {
            workpiece: wp(name),
            closure,
            base_closure: ClosureKey::from_digest(Digest::from_bytes([fill.wrapping_add(1); 32])),
            declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
        }
    }

    fn wp(name: &str) -> WorkpieceId {
        WorkpieceId(name.to_owned())
    }

    fn green_facts(ids: impl IntoIterator<Item = impl Into<String>>) -> DiscriminatedFacts {
        let mut first = RunnerReport::new();
        let mut second = RunnerReport::new();
        for id in ids {
            let id = id.into();
            first.insert(id.clone(), ProofResult::Green);
            second.insert(id, ProofResult::Green);
        }
        discriminate(&first, &second)
    }

    fn ctx<'a>(store: &'a mut SqliteStore, host: &'a HostClass) -> BatchContext<'a> {
        BatchContext {
            store,
            host_class: host,
            producing_dispatch: "nonce-batch",
            producing_bloom: &BLOOM,
            base_commit: BASE_COMMIT,
        }
    }

    fn seed_green(store: &mut dyn StoreBackend, closure: ClosureKey) {
        let facts = green_facts(["alpha::broke"]);
        record_proof_facts(
            store,
            &ProofSource::Member { closure },
            &facts,
            &HostClass::new("fleet"),
            "nonce-seed",
            &BLOOM,
        )
        .expect("a seeded fact writes");
    }

    struct ScriptedGate {
        runs: usize,
        outcome: GateOutcome,
    }

    impl BatchGate for ScriptedGate {
        fn run(&mut self, _members: &[BatchMember]) -> GateOutcome {
            self.runs += 1;
            self.outcome.clone()
        }
    }

    struct ScriptedProbe {
        runs: usize,
    }

    impl BaseProbe for ScriptedProbe {
        fn run(&mut self, test_id: &str) -> RunnerReport {
            self.runs += 1;
            let mut report = RunnerReport::new();
            report.insert(test_id, ProofResult::Green);
            report
        }
    }

    #[derive(Default)]
    struct RecordingBoard {
        filed: Vec<BaseRepairWorkpiece>,
    }

    impl RepairBoard for RecordingBoard {
        fn file(&mut self, repair: &BaseRepairWorkpiece) -> Result<(), AttributionError> {
            self.filed.push(repair.clone());
            Ok(())
        }
    }

    struct ScriptedBisect {
        hits: Vec<WorkpieceId>,
    }

    impl BatchBisect for ScriptedBisect {
        fn fails(&mut self, subset: &[BatchMember]) -> bool {
            subset.iter().any(|member| self.hits.contains(&member.workpiece))
        }
    }
}
