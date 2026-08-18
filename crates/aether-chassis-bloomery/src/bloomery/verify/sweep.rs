//! Daily sweeps convert unknown facts on idle prover time (ADR-0200
//! §"The gate ladder").
//!
//! A land invalidates the closures it touches. Those keys sit unknown
//! until an idle slot — no lane occupying the builder — converts them
//! cheapest-first. Discrimination guards the write, the same as every
//! other producer. A red taints its closure, files a repair workpiece
//! in the #5128 body shape, and holds any bloom whose members touch
//! that closure. The culprit is the land that introduced the red,
//! found by bisecting the day's linear order with one test, never the
//! suite.

use super::{
    AttributionError, BaseProbe, BaseRepairWorkpiece, ClosureKey, DiscriminatedFacts, HostClass, ProofResult,
    ProofSource, RepairBoard, TaintSet, consult_proof_fact, discriminate, record_proof_facts,
};
use crate::store::StoreBackend;

/// One closure key a land invalidated that the ledger has not yet converted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnknownFact {
    /// The post-land closure the land invalidated.
    pub closure: ClosureKey,
    /// The test that has no fact at this key yet.
    pub test_id: String,
    /// Cheapest-first key. Smaller runs first (typically mean handler nanos).
    pub cost_nanos: u64,
}

/// What the scheduler should do with idle prover time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SweepDecision {
    /// A lane occupies a build slot, or nothing is unknown.
    Idle,
    /// Convert this unknown — the cheapest waiting fact.
    Convert(UnknownFact),
}

/// What one sweep conversion produced after discrimination.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SweepOutcome {
    /// Both runs were green. The unknown is now a green fact.
    Green,
    /// Both runs were red. The closure is tainted and a repair is filed.
    Red,
    /// Discrimination dropped the observation. Nothing is recorded.
    Flake,
}

/// Whether a bloom may seal or dispatch against the current taint set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BloomDisposition {
    /// No member closure is tainted.
    Proceed,
    /// A member touches a tainted closure — hold at seal and dispatch.
    Hold {
        /// The tainted closure the bloom touches.
        closure: ClosureKey,
    },
}

/// One commit in the day's linear land order (ADR-0186).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Land {
    /// The landed commit. Identity in the day's sequence is slice position.
    pub commit: String,
}

/// Inputs the sweep shares with the ledger write.
pub struct SweepContext<'a> {
    /// The proof-fact ledger.
    pub store: &'a mut dyn StoreBackend,
    /// Host class facts are keyed on.
    pub host_class: &'a HostClass,
    /// Dispatch stamped on any fact this conversion records.
    pub producing_dispatch: &'a str,
    /// Bloom stamped on any fact this conversion records.
    pub producing_bloom: &'a [u8],
    /// Day-head commit the repair body names as its base.
    pub day_commit: &'a str,
}

/// One execution of one test at one land. Never the suite.
pub trait LandProbe {
    /// Run `test_id` at `land` and return that one result.
    fn run(&mut self, land: &Land, test_id: &str) -> ProofResult;
}

/// Unknowns among `invalidated`, cheapest first.
///
/// A key the ledger already holds a fact for is converted. The rest wait.
///
/// # Errors
/// The ledger read failed.
pub fn unknowns(
    store: &mut dyn StoreBackend,
    host: &HostClass,
    invalidated: &[UnknownFact],
) -> rusqlite::Result<Vec<UnknownFact>> {
    let mut waiting = Vec::new();
    for fact in invalidated {
        if consult_proof_fact(store, &fact.closure, &fact.test_id, host)?.is_none() {
            waiting.push(fact.clone());
        }
    }
    waiting.sort_by_key(|fact| fact.cost_nanos);
    Ok(waiting)
}

/// Convert the cheapest unknown only when no lane occupies a build slot.
#[must_use]
pub fn decide_sweep(occupied_slots: usize, waiting: &[UnknownFact]) -> SweepDecision {
    match (occupied_slots, waiting.first()) {
        (0, Some(fact)) => SweepDecision::Convert(fact.clone()),
        _ => SweepDecision::Idle,
    }
}

/// Whether `member_closures` may seal or dispatch.
#[must_use]
pub fn bloom_disposition(taints: &TaintSet, member_closures: &[ClosureKey]) -> BloomDisposition {
    member_closures
        .iter()
        .find(|closure| taints.is_tainted(closure))
        .copied()
        .map_or(BloomDisposition::Proceed, |closure| BloomDisposition::Hold { closure })
}

/// Release the taint the repair just landed against.
pub fn repair_landed(taints: &mut TaintSet, closure: &ClosureKey) {
    taints.release(closure);
}

/// Convert one unknown: two single-test runs, discriminate, record, and
/// on red taint the closure and file a #5128-shaped repair.
///
/// # Errors
/// The ledger could not be written, or the board refused the repair.
pub fn run_sweep(
    ctx: &mut SweepContext<'_>,
    unknown: &UnknownFact,
    probe: &mut dyn BaseProbe,
    board: &mut dyn RepairBoard,
    taints: &mut TaintSet,
) -> Result<SweepOutcome, AttributionError> {
    let facts = discriminate(&probe.run(&unknown.test_id), &probe.run(&unknown.test_id));
    record_proof_facts(
        ctx.store,
        &ProofSource::Sweep { closure: unknown.closure },
        &facts,
        ctx.host_class,
        ctx.producing_dispatch,
        ctx.producing_bloom,
    )?;
    match swept_result(&facts, &unknown.test_id) {
        Some(ProofResult::Red) => {
            taints.taint(unknown.closure, unknown.test_id.clone());
            board.file(&BaseRepairWorkpiece {
                test_id: unknown.test_id.clone(),
                base_commit: ctx.day_commit.to_owned(),
                closure_key: unknown.closure,
            })?;
            Ok(SweepOutcome::Red)
        }
        Some(ProofResult::Green) => Ok(SweepOutcome::Green),
        None => Ok(SweepOutcome::Flake),
    }
}

/// Find the land that introduced a sweep-red test.
///
/// `lands` is the day's order, oldest first. The last land is the day
/// head the sweep already proved red, so it is not re-run. Each probe
/// is one test at one land.
#[must_use]
pub fn bisect_land_order<'a>(lands: &'a [Land], test_id: &str, probe: &mut dyn LandProbe) -> Option<&'a Land> {
    if lands.is_empty() {
        return None;
    }
    let mut lo = 0;
    let mut hi = lands.len() - 1;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if probe.run(&lands[mid], test_id) == ProofResult::Red {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Some(&lands[lo])
}

fn swept_result(facts: &DiscriminatedFacts, test_id: &str) -> Option<ProofResult> {
    facts.iter().find(|fact| fact.test_id == test_id).map(|fact| fact.result)
}

#[cfg(test)]
mod tests {
    use aether_bloomery::Digest;

    use super::{
        BaseProbe, BaseRepairWorkpiece, BloomDisposition, Land, LandProbe, RepairBoard, SweepContext, SweepDecision,
        SweepOutcome, UnknownFact, bisect_land_order, bloom_disposition, decide_sweep, repair_landed, run_sweep,
        unknowns,
    };
    use crate::bloomery::verify::{
        Attribution, AttributionError, AttributionRequest, ClosureKey, HostClass, ProofResult, RunnerReport, TaintSet,
        attribute_gate_failure,
    };
    use crate::store::{SqliteStore, StoreBackend};

    const TEST_ID: &str = "crate::swept";
    const DAY_COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_sweep_red_holds_a_touching_bloom_and_releases_when_the_repair_lands() {
        // A post-land red is discovered after the land, by design. Taint
        // plus auto-repair is what bounds it: a bloom that touches the
        // closure must not seal or dispatch until the repair lands, and
        // a bloom that does not touch it is not held.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let host = HostClass::new("fleet");
        let tainted = key(0x51);
        let untouched = key(0x52);
        let cheap = unknown(tainted, TEST_ID, 10);
        let expensive = unknown(tainted, "crate::other", 1_000);
        let waiting = unknowns(&mut store, &host, &[expensive, cheap.clone()]).expect("the ledger starts empty");

        assert_eq!(decide_sweep(1, &waiting), SweepDecision::Idle, "a live lane keeps the prover");
        assert_eq!(
            decide_sweep(0, &waiting),
            SweepDecision::Convert(cheap.clone()),
            "idle time converts cheapest-first"
        );

        let mut probe = ScriptedProbe::new([ProofResult::Red, ProofResult::Red]);
        let mut board = RecordingBoard::default();
        let mut taints = TaintSet::new();
        let outcome = run_sweep(&mut ctx(&mut store, &host), &cheap, &mut probe, &mut board, &mut taints)
            .expect("a red converts");

        assert_eq!(outcome, SweepOutcome::Red);
        assert_eq!(probe.runs, 2, "discrimination is two single-test runs, not the suite");
        assert_eq!(board.filed.len(), 1, "a sweep red files exactly one repair");
        let body = board.filed[0].body();
        assert!(body.contains(TEST_ID), "the body names the failing test");
        assert!(body.contains(DAY_COMMIT), "the body names the day-head commit");
        assert!(body.contains(&closure_hex(&tainted)), "the body names the closure key");

        let rows = store.list_proof_facts().expect("the table reads");
        assert_eq!(rows.len(), 1, "discrimination recorded one fact");
        assert_eq!(rows[0].result, "red");
        assert!(
            unknowns(&mut store, &host, std::slice::from_ref(&cheap)).expect("the read").is_empty(),
            "a converted key is no longer unknown"
        );

        assert_eq!(
            bloom_disposition(&taints, &[tainted, untouched]),
            BloomDisposition::Hold { closure: tainted },
            "a bloom that touches the tainted closure holds"
        );
        assert_eq!(
            bloom_disposition(&taints, &[untouched]),
            BloomDisposition::Proceed,
            "a bloom that does not touch it is not held"
        );

        let attribution = attribute_gate_failure(
            &mut store,
            &request(tainted, &host),
            &mut ScriptedProbe::new([]),
            &mut RecordingBoard::default(),
            &taints,
        )
        .expect("taint consults");
        assert_eq!(attribution, Attribution::Predating, "taint is consulted before anyone is charged");
        assert!(!attribution.charges_refine_lap());

        repair_landed(&mut taints, &tainted);
        assert_eq!(
            bloom_disposition(&taints, &[tainted]),
            BloomDisposition::Proceed,
            "the repair landing releases the hold"
        );
    }

    #[test]
    fn bisect_finds_a_planted_culprit_with_single_test_executions_only() {
        // Eight lands, the fifth introduced the red. Linear search would
        // run the test at every land; the rung is O(log lands) and never
        // the suite — HEAD is already known red from the sweep.
        let lands: Vec<Land> = (0..8).map(|index| Land { commit: format!("land-{index}") }).collect();
        let culprit = 5;
        let mut probe = Planted { culprit, runs: Vec::new() };

        let found = bisect_land_order(&lands, TEST_ID, &mut probe).expect("a red HEAD has a first-red land");

        assert_eq!(found.commit, "land-5", "the planted land is the first red");
        assert!(probe.runs.len() < lands.len(), "bisect must not walk the day linearly; ran {}", probe.runs.len());
        assert!(
            probe.runs.iter().all(|(_, test)| test == TEST_ID),
            "every execution is the one failing test, never the suite"
        );
        assert!(probe.runs.iter().all(|(commit, _)| commit != "land-7"), "HEAD is already known red and is not re-run");
    }

    fn unknown(closure: ClosureKey, test_id: &str, cost_nanos: u64) -> UnknownFact {
        UnknownFact { closure, test_id: test_id.to_owned(), cost_nanos }
    }

    fn key(fill: u8) -> ClosureKey {
        ClosureKey::from_digest(Digest::from_bytes([fill; 32]))
    }

    fn ctx<'a>(store: &'a mut SqliteStore, host: &'a HostClass) -> SweepContext<'a> {
        SweepContext {
            store,
            host_class: host,
            producing_dispatch: "nonce-sweep",
            producing_bloom: &[0x50; 32],
            day_commit: DAY_COMMIT,
        }
    }

    fn request(closure: ClosureKey, host: &HostClass) -> AttributionRequest<'_> {
        AttributionRequest {
            base_closure: closure,
            test_id: TEST_ID,
            host_class: host,
            base_commit: DAY_COMMIT,
            producing_dispatch: "nonce-attr",
            producing_bloom: &[0xA0; 32],
        }
    }

    fn closure_hex(key: &ClosureKey) -> String {
        key.as_bytes().iter().fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    struct ScriptedProbe {
        outcomes: Vec<ProofResult>,
        runs: usize,
    }

    impl ScriptedProbe {
        fn new(outcomes: impl Into<Vec<ProofResult>>) -> Self {
            Self { outcomes: outcomes.into(), runs: 0 }
        }
    }

    impl BaseProbe for ScriptedProbe {
        fn run(&mut self, test_id: &str) -> RunnerReport {
            let result = self.outcomes.get(self.runs).copied().expect("the script has a result for this run");
            self.runs += 1;
            let mut report = RunnerReport::new();
            report.insert(test_id, result);
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

    struct Planted {
        culprit: usize,
        runs: Vec<(String, String)>,
    }

    impl LandProbe for Planted {
        fn run(&mut self, land: &Land, test_id: &str) -> ProofResult {
            self.runs.push((land.commit.clone(), test_id.to_owned()));
            let index = land.commit.strip_prefix("land-").and_then(|rest| rest.parse::<usize>().ok()).expect("land-N");
            if index >= self.culprit {
                ProofResult::Red
            } else {
                ProofResult::Green
            }
        }
    }
}
