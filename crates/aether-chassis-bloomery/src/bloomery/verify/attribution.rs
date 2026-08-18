//! Attribution of a gate failure through the proof-fact ledger (ADR-0200
//! §"Attribution through the ledger").
//!
//! A named failing test resolves four ways. A sweep taint on the closure
//! is consulted first and charges no one — the daily sweep already owns
//! that red. A green fact at the base closure is certain — the member's
//! diff broke it, and nothing is rerun. No fact means the base is probed
//! with that one test; the probe is discriminated and recorded before it
//! is believed. A red probe predates the member: a base-repair workpiece
//! is filed onto the board as an ordinary issue, and the member is not
//! charged a refine lap.
//!
//! ADR-0195 failure classes compose by not entering this ladder: the
//! caller invokes it only for a named test failure (`ArtifactRejected`).
//! A machinery observation never consults, never probes, and never
//! charges a model lap.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use super::{
    ClosureKey, DiscriminatedFacts, HostClass, ProofResult, ProofSource, RunnerReport, discriminate, record_proof_facts,
};
use crate::store::StoreBackend;

/// Closures a sweep-discovered red has tainted (ADR-0200 §The gate ladder).
///
/// A red from the daily sweep taints its closure: blooms whose members touch
/// it hold at seal and dispatch, and the attribution ladder consults the
/// taint before charging anyone. A repair landing releases the taint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaintSet {
    tainted: BTreeMap<ClosureKey, String>,
}

impl TaintSet {
    /// An empty set — nothing holds, nothing is predating via taint.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `closure` tainted by the named test.
    pub fn taint(&mut self, closure: ClosureKey, test_id: impl Into<String>) {
        self.tainted.insert(closure, test_id.into());
    }

    /// Drop the taint on `closure` — the repair has landed.
    pub fn release(&mut self, closure: &ClosureKey) {
        self.tainted.remove(closure);
    }

    /// Whether this closure is currently tainted.
    #[must_use]
    pub fn is_tainted(&self, closure: &ClosureKey) -> bool {
        self.tainted.contains_key(closure)
    }
}

/// Who a gate failure is charged to after the ledger has been consulted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Attribution {
    /// The member's diff broke a test the base had green — or the base
    /// probe just proved green. Charge a refine lap.
    Member,
    /// The failure predates the member. Charge no refine lap.
    Predating,
}

impl Attribution {
    /// Whether this resolution spends a member refine lap.
    #[must_use]
    pub fn charges_refine_lap(self) -> bool {
        matches!(self, Self::Member)
    }
}

/// Why attribution could not resolve.
#[derive(Debug)]
pub enum AttributionError {
    /// The ledger read or the fact write failed.
    Store(rusqlite::Error),
    /// The board refused the base-repair workpiece.
    Board(String),
}

impl Display for AttributionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "proof-fact ledger error: {error}"),
            Self::Board(error) => write!(f, "base-repair workpiece could not be filed: {error}"),
        }
    }
}

impl Error for AttributionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Board(_) => None,
        }
    }
}

impl From<rusqlite::Error> for AttributionError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

/// The base-side identity of one failing test the ladder attributes.
pub struct AttributionRequest<'a> {
    /// The base checkout's closure key for the test's package.
    pub base_closure: ClosureKey,
    /// The one failing test being attributed.
    pub test_id: &'a str,
    /// The host class the candidate failed on — facts do not travel
    /// across host classes (ADR-0200 integrity rule 2).
    pub host_class: &'a HostClass,
    /// The base commit the probe would run at, and the repair body names.
    pub base_commit: &'a str,
    /// The dispatch that is attributing, stamped on any fact the probe records.
    pub producing_dispatch: &'a str,
    /// The bloom that is attributing, stamped on any fact the probe records.
    pub producing_bloom: &'a [u8],
}

/// A base-repair workpiece: an ordinary board issue, never a bloom member.
///
/// The body carries the failing test, the base commit, and the closure key
/// so a later bloom can pick the work up through ordinary staging.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BaseRepairWorkpiece {
    /// The test that is red at the base.
    pub test_id: String,
    /// The base commit the probe ran against.
    pub base_commit: String,
    /// The base closure the red fact is addressed by.
    pub closure_key: ClosureKey,
}

impl BaseRepairWorkpiece {
    /// The issue body a board files. Load-bearing contents: the failing
    /// test, the base commit, and the closure key.
    #[must_use]
    pub fn body(&self) -> String {
        format!(
            "failing test: {}\nbase commit: {}\nclosure key: {}\n",
            self.test_id,
            self.base_commit,
            closure_hex(&self.closure_key),
        )
    }
}

/// Run one failing test at the base checkout — never the suite.
///
/// The ladder calls this twice and discriminates; a single call is not a
/// fact. The implementation holds the checkout; this trait only names the
/// test.
pub trait BaseProbe {
    /// One independent run of `test_id` at the base.
    fn run(&mut self, test_id: &str) -> RunnerReport;
}

/// File a base-repair workpiece onto the issue board.
///
/// The workpiece enters as an ordinary issue. It is not sealed into the
/// bloom that discovered it.
pub trait RepairBoard {
    /// Persist `repair` as a board issue.
    ///
    /// # Errors
    /// The board could not file the issue.
    fn file(&mut self, repair: &BaseRepairWorkpiece) -> Result<(), AttributionError>;
}

/// The latest discriminated fact at `(closure, test, host_class)`, or
/// `None` when the ledger has never recorded one.
///
/// # Errors
/// The ledger read failed.
pub fn consult_proof_fact(
    store: &mut dyn StoreBackend,
    closure: &ClosureKey,
    test_id: &str,
    host_class: &HostClass,
) -> rusqlite::Result<Option<ProofResult>> {
    let key = closure.as_bytes().as_slice();
    let host = host_class.as_str();
    Ok(store
        .list_proof_facts()?
        .into_iter()
        .filter(|row| row.closure_key == key && row.test_id == test_id && row.host_class == host)
        .filter_map(|row| ProofResult::from_stored(&row.result))
        .next_back())
}

/// Attribute one named test failure at member verify or the aggregate gate.
///
/// A tainted closure is predating — the sweep already owns the red, so
/// nobody is charged. Certain green issues no probe. A missing fact
/// probes the base twice, records what discrimination accepted, and
/// charges the member if the base is green. A red probe files a
/// base-repair workpiece and charges no lap.
///
/// # Errors
/// The ledger could not be read or written, or the board refused the repair.
pub fn attribute_gate_failure(
    store: &mut dyn StoreBackend,
    request: &AttributionRequest<'_>,
    probe: &mut dyn BaseProbe,
    board: &mut dyn RepairBoard,
    taints: &TaintSet,
) -> Result<Attribution, AttributionError> {
    if taints.is_tainted(&request.base_closure) {
        return Ok(Attribution::Predating);
    }
    match consult_proof_fact(store, &request.base_closure, request.test_id, request.host_class)? {
        Some(ProofResult::Green) => Ok(Attribution::Member),
        Some(ProofResult::Red) => Ok(Attribution::Predating),
        None => resolve_probe(store, request, probe, board),
    }
}

fn resolve_probe(
    store: &mut dyn StoreBackend,
    request: &AttributionRequest<'_>,
    probe: &mut dyn BaseProbe,
    board: &mut dyn RepairBoard,
) -> Result<Attribution, AttributionError> {
    let first = probe.run(request.test_id);
    let second = probe.run(request.test_id);
    let facts = discriminate(&first, &second);
    record_proof_facts(
        store,
        &ProofSource::Member { closure: request.base_closure },
        &facts,
        request.host_class,
        request.producing_dispatch,
        request.producing_bloom,
    )?;

    match probed_result(&facts, request.test_id) {
        Some(ProofResult::Red) => {
            board.file(&BaseRepairWorkpiece {
                test_id: request.test_id.to_owned(),
                base_commit: request.base_commit.to_owned(),
                closure_key: request.base_closure,
            })?;
            Ok(Attribution::Predating)
        }
        // A green probe, or a flake discrimination dropped: charge the
        // member. Uncertain falls through to today's ArtifactRejected
        // response (ADR-0195) rather than letting a broken candidate off.
        Some(ProofResult::Green) | None => Ok(Attribution::Member),
    }
}

fn probed_result(facts: &DiscriminatedFacts, test_id: &str) -> Option<ProofResult> {
    facts.iter().find(|fact| fact.test_id == test_id).map(|fact| fact.result)
}

fn closure_hex(key: &ClosureKey) -> String {
    key.as_bytes().iter().fold(String::with_capacity(64), |mut hex, byte| {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

#[cfg(test)]
mod tests {
    use aether_bloomery::Digest;

    use super::{
        Attribution, AttributionRequest, BaseProbe, BaseRepairWorkpiece, RepairBoard, TaintSet, attribute_gate_failure,
    };
    use crate::bloomery::verify::{
        ClosureKey, HostClass, ProofResult, ProofSource, RunnerReport, discriminate, record_proof_facts,
    };
    use crate::store::{SqliteStore, StoreBackend};

    const TEST_ID: &str = "crate::the_failing_test";
    const BASE_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn a_green_base_fact_charges_the_member_without_rerunning() {
        // Certain attribution: the ledger already holds a green at the
        // base closure. The member broke it. A probe would only re-derive
        // what the fact already says.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let closure = key(0x11);
        let host = HostClass::new("fleet");
        seed(&mut store, closure, ProofResult::Green, &host);

        let mut probe = ScriptedProbe::new([]);
        let mut board = RecordingBoard::default();
        let attribution =
            attribute_gate_failure(&mut store, &request(closure, &host), &mut probe, &mut board, &TaintSet::new())
                .expect("certain green resolves");

        assert_eq!(attribution, Attribution::Member);
        assert!(attribution.charges_refine_lap(), "a broken-by-diff green charges the member");
        assert_eq!(probe.runs, 0, "a green fact at the base issues no rerun");
        assert!(board.filed.is_empty(), "certain member attribution files no repair");
        assert_eq!(store.list_proof_facts().expect("the table reads").len(), 1, "consultation does not append");
    }

    #[test]
    fn a_missing_fact_probes_the_base_and_records_what_it_proved() {
        // No fact: the ladder must ask the base, discriminate the two
        // runs, and persist what they agreed on. A green probe means the
        // member broke a test the base still passes.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let closure = key(0x22);
        let host = HostClass::new("fleet");
        let mut probe = ScriptedProbe::new([ProofResult::Green, ProofResult::Green]);
        let mut board = RecordingBoard::default();

        let attribution =
            attribute_gate_failure(&mut store, &request(closure, &host), &mut probe, &mut board, &TaintSet::new())
                .expect("a green probe resolves");

        assert_eq!(attribution, Attribution::Member);
        assert_eq!(probe.runs, 2, "discrimination is two independent base runs, not the suite");
        assert!(board.filed.is_empty(), "a green base is the member's to repair");

        let rows = store.list_proof_facts().expect("the table reads");
        assert_eq!(rows.len(), 1, "the probe records exactly one discriminated fact");
        assert_eq!(rows[0].closure_key, closure.as_bytes());
        assert_eq!(rows[0].test_id, TEST_ID);
        assert_eq!(rows[0].result, "green");
        assert_eq!(rows[0].host_class, "fleet");
    }

    #[test]
    fn a_red_base_probe_files_a_repair_workpiece_and_charges_no_lap() {
        // The issue-5020 class: the test is already red at the base, so
        // the member did not introduce it. File a board issue and do not
        // spend a refine lap on a candidate that cannot fix the base.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let closure = key(0x33);
        let host = HostClass::new("fleet");
        let mut probe = ScriptedProbe::new([ProofResult::Red, ProofResult::Red]);
        let mut board = RecordingBoard::default();

        let attribution =
            attribute_gate_failure(&mut store, &request(closure, &host), &mut probe, &mut board, &TaintSet::new())
                .expect("a red probe resolves");

        assert_eq!(attribution, Attribution::Predating);
        assert!(!attribution.charges_refine_lap(), "a predating red charges no refine lap");
        assert_eq!(probe.runs, 2, "the base is probed, not the suite");
        assert_eq!(board.filed.len(), 1, "exactly one base-repair workpiece is filed");

        let repair = &board.filed[0];
        assert_eq!(repair.test_id, TEST_ID);
        assert_eq!(repair.base_commit, BASE_COMMIT);
        assert_eq!(repair.closure_key, closure);
        let body = repair.body();
        let closure_hex = super::closure_hex(&closure);
        assert!(body.contains(TEST_ID), "the body names the failing test");
        assert!(body.contains(BASE_COMMIT), "the body names the base commit");
        assert!(body.contains(&closure_hex), "the body names the closure key");

        let rows = store.list_proof_facts().expect("the table reads");
        assert_eq!(rows.len(), 1, "the red probe is recorded as a fact");
        assert_eq!(rows[0].result, "red");
    }

    fn key(fill: u8) -> ClosureKey {
        ClosureKey::from_digest(Digest::from_bytes([fill; 32]))
    }

    fn request(closure: ClosureKey, host: &HostClass) -> AttributionRequest<'_> {
        AttributionRequest {
            base_closure: closure,
            test_id: TEST_ID,
            host_class: host,
            base_commit: BASE_COMMIT,
            producing_dispatch: "nonce-attr",
            producing_bloom: &[0xA0; 32],
        }
    }

    fn seed(store: &mut dyn StoreBackend, closure: ClosureKey, result: ProofResult, host: &HostClass) {
        let mut first = RunnerReport::new();
        first.insert(TEST_ID, result);
        let mut second = RunnerReport::new();
        second.insert(TEST_ID, result);
        record_proof_facts(
            store,
            &ProofSource::Member { closure },
            &discriminate(&first, &second),
            host,
            "nonce-seed",
            &[0x50; 32],
        )
        .expect("a seeded fact writes");
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
        fn file(&mut self, repair: &BaseRepairWorkpiece) -> Result<(), super::AttributionError> {
            self.filed.push(repair.clone());
            Ok(())
        }
    }
}
