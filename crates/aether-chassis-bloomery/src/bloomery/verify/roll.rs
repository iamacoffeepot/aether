//! The roll barrier (ADR-0200 §"The gate ladder"): main receives the day's
//! tree only when the coverage map is fully green.
//!
//! The gate reads the ledger's coverage of the day head. Every test
//! closure must carry a host-class-matched green fact at the head's
//! closure key. A hold names exactly what is missing — unknowns queue as
//! priority sweeps, reds point at their taint/repair workpiece — so the
//! operator sees a work list, not a refusal. Train drain, dirty-tree,
//! and ratification (batched ADR mode) stay on the existing roll screen;
//! this is only the coverage condition.

use std::fmt::{self, Display, Formatter};

use super::{BaseRepairWorkpiece, ClosureKey, HostClass, ProofResult, UnknownFact, consult_proof_fact};
use crate::store::StoreBackend;

/// One test the day head must prove, addressed by its package closure.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TestClosure {
    /// The day-head closure key this test is addressed by.
    pub closure: ClosureKey,
    /// The test that must carry a green fact at that key.
    pub test_id: String,
}

/// Ledger status of one test closure at the day head, host-class-matched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoverageStatus {
    /// A green fact exists at this key on this host class.
    Green,
    /// A red fact exists at this key on this host class.
    Red,
    /// The ledger has no fact at this key on this host class.
    Unknown,
}

/// One required test closure and what the ledger says about it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CoverageEntry {
    /// The day-head closure key.
    pub closure: ClosureKey,
    /// The test this row covers.
    pub test_id: String,
    /// Host-class-matched fact at that address, or its absence.
    pub status: CoverageStatus,
}

/// The ledger's coverage of the day head: every required test closure
/// and whether a host-class-matched fact exists at the head's key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageMap {
    entries: Vec<CoverageEntry>,
}

impl CoverageMap {
    /// Each required closure in the order the caller supplied it.
    pub fn iter(&self) -> impl Iterator<Item = &CoverageEntry> {
        self.entries.iter()
    }

    /// Whether every required closure carries a green fact.
    #[must_use]
    pub fn is_fully_green(&self) -> bool {
        self.entries.iter().all(|entry| entry.status == CoverageStatus::Green)
    }
}

/// Why one required closure is not green.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MissingCoverage {
    /// No fact yet — queue this as a priority sweep.
    Unknown {
        /// The day-head closure still waiting for a fact.
        closure: ClosureKey,
        /// The test the sweep should convert.
        test_id: String,
    },
    /// A red fact — the repair workpiece the operator should land.
    Red {
        /// The taint/repair workpiece for this red, same body as a sweep files.
        repair: BaseRepairWorkpiece,
    },
}

/// The named work list a held roll hands the operator.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RollHold {
    missing: Vec<MissingCoverage>,
}

impl RollHold {
    /// Every missing closure, in the order the coverage map supplied it.
    #[must_use]
    pub fn missing(&self) -> &[MissingCoverage] {
        &self.missing
    }

    /// Unknown closures, queued cheapest-first as priority for the sweep.
    #[must_use]
    pub fn priority_sweeps(&self) -> Vec<UnknownFact> {
        self.missing
            .iter()
            .filter_map(|missing| match missing {
                MissingCoverage::Unknown { closure, test_id } => {
                    Some(UnknownFact { closure: *closure, test_id: test_id.clone(), cost_nanos: 0 })
                }
                MissingCoverage::Red { .. } => None,
            })
            .collect()
    }
}

impl Display for RollHold {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (index, missing) in self.missing.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            match missing {
                MissingCoverage::Unknown { closure, test_id } => {
                    write!(f, "unknown test {test_id} at {} — queue as priority sweep", closure_hex(closure))?;
                }
                MissingCoverage::Red { repair } => write!(
                    f,
                    "red test {} at {} — repair workpiece:\n{}",
                    repair.test_id,
                    closure_hex(&repair.closure_key),
                    repair.body()
                )?,
            }
        }
        Ok(())
    }
}

/// Whether the day may roll onto main.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RollDecision {
    /// Every required closure is green on this host class.
    Release,
    /// Something is missing. The hold is the work list.
    Hold(RollHold),
}

/// Build the coverage map for `required` at the day head on `host`.
///
/// A fact recorded on another host class does not cover this one.
/// Latest discriminated row at the address wins, same as consultation.
///
/// # Errors
/// The ledger read failed.
pub fn coverage_map(
    store: &mut dyn StoreBackend,
    host: &HostClass,
    required: &[TestClosure],
) -> rusqlite::Result<CoverageMap> {
    let mut entries = Vec::with_capacity(required.len());
    for test in required {
        let status = match consult_proof_fact(store, &test.closure, &test.test_id, host)? {
            Some(ProofResult::Green) => CoverageStatus::Green,
            Some(ProofResult::Red) => CoverageStatus::Red,
            None => CoverageStatus::Unknown,
        };
        entries.push(CoverageEntry { closure: test.closure, test_id: test.test_id.clone(), status });
    }
    Ok(CoverageMap { entries })
}

/// The coverage condition of the day roll. Other roll screens — drain,
/// dirty tree, ratification — are orthogonal and are not consulted here.
#[must_use]
pub fn decide_roll(map: &CoverageMap, day_commit: &str) -> RollDecision {
    let missing: Vec<MissingCoverage> = map
        .entries
        .iter()
        .filter_map(|entry| match entry.status {
            CoverageStatus::Green => None,
            CoverageStatus::Unknown => {
                Some(MissingCoverage::Unknown { closure: entry.closure, test_id: entry.test_id.clone() })
            }
            CoverageStatus::Red => Some(MissingCoverage::Red {
                repair: BaseRepairWorkpiece {
                    test_id: entry.test_id.clone(),
                    base_commit: day_commit.to_owned(),
                    closure_key: entry.closure,
                },
            }),
        })
        .collect();
    if missing.is_empty() {
        RollDecision::Release
    } else {
        RollDecision::Hold(RollHold { missing })
    }
}

fn closure_hex(key: &ClosureKey) -> String {
    aether_bloomery::encode_hex(key.as_bytes())
}

#[cfg(test)]
mod tests {
    use aether_bloomery::Digest;

    use super::{CoverageStatus, MissingCoverage, RollDecision, TestClosure, coverage_map, decide_roll};
    use crate::bloomery::verify::{
        ClosureKey, HostClass, ProofResult, ProofSource, RunnerReport, SweepDecision, decide_sweep, discriminate,
        record_proof_facts,
    };
    use crate::store::{SqliteStore, StoreBackend};

    const TEST_ID: &str = "crate::day_head";
    const DAY_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn the_roll_holds_on_an_unknown_fact_names_it_and_releases_when_it_turns_green() {
        // The day cannot reach main with an unconverted fact. The barrier
        // holds, names the missing closure so it queues as a priority sweep,
        // and only releases once that fact is green at the head's key on
        // this host class — a green on another host is not coverage.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let fleet = HostClass::new("fleet");
        let gpu = HostClass::new("gpu");
        let closure = key(0x61);
        let required = [TestClosure { closure, test_id: TEST_ID.to_owned() }];

        seed(&mut store, closure, ProofResult::Green, &gpu);

        let map = coverage_map(&mut store, &fleet, &required).expect("the ledger reads");
        assert_eq!(map.iter().next().expect("one required closure").status, CoverageStatus::Unknown);
        assert!(!map.is_fully_green(), "a green on another host class is not coverage");

        let RollDecision::Hold(hold) = decide_roll(&map, DAY_COMMIT) else {
            panic!("an unknown fact must hold the roll");
        };
        assert_eq!(
            hold.missing(),
            [MissingCoverage::Unknown { closure, test_id: TEST_ID.to_owned() }],
            "the hold names the missing closure, not a bare refusal"
        );
        let named = hold.to_string();
        assert!(named.contains(TEST_ID), "the work list names the unknown test: {named}");
        assert!(named.contains("priority sweep"), "the unknown queues as a priority sweep: {named}");

        let waiting = hold.priority_sweeps();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].test_id, TEST_ID);
        assert_eq!(
            decide_sweep(0, &waiting),
            SweepDecision::Convert(waiting[0].clone()),
            "the named unknown queues through the existing sweep scheduler"
        );

        seed(&mut store, closure, ProofResult::Green, &fleet);

        let map = coverage_map(&mut store, &fleet, &required).expect("the ledger reads");
        assert!(map.is_fully_green(), "a host-class-matched green is coverage");
        assert_eq!(decide_roll(&map, DAY_COMMIT), RollDecision::Release, "the green fact releases the hold");
    }

    #[test]
    fn a_red_fact_holds_the_roll_and_points_at_the_repair_workpiece() {
        // A sweep-discovered red is already a fact; the roll must not treat
        // it as coverage. The hold points at the same repair workpiece the
        // sweep filed, so the operator has a work item rather than a wall.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let host = HostClass::new("fleet");
        let closure = key(0x62);
        let required = [TestClosure { closure, test_id: TEST_ID.to_owned() }];
        seed(&mut store, closure, ProofResult::Red, &host);

        let map = coverage_map(&mut store, &host, &required).expect("the ledger reads");
        let RollDecision::Hold(hold) = decide_roll(&map, DAY_COMMIT) else {
            panic!("a red fact must hold the roll");
        };
        let MissingCoverage::Red { repair } = &hold.missing()[0] else {
            panic!("a red fact points at its repair, not an unknown: {:?}", hold.missing());
        };
        assert_eq!(repair.test_id, TEST_ID);
        assert_eq!(repair.base_commit, DAY_COMMIT);
        assert_eq!(repair.closure_key, closure);
        let named = hold.to_string();
        assert!(named.contains(TEST_ID), "the work list names the red test: {named}");
        assert!(named.contains(DAY_COMMIT), "the work list names the day-head commit: {named}");
        assert!(hold.priority_sweeps().is_empty(), "a red is a repair, not another sweep");
    }

    fn key(fill: u8) -> ClosureKey {
        ClosureKey::from_digest(Digest::from_bytes([fill; 32]))
    }

    fn seed(store: &mut dyn StoreBackend, closure: ClosureKey, result: ProofResult, host: &HostClass) {
        let mut first = RunnerReport::new();
        first.insert(TEST_ID, result);
        let mut second = RunnerReport::new();
        second.insert(TEST_ID, result);
        record_proof_facts(
            store,
            &ProofSource::Sweep { closure },
            &discriminate(&first, &second),
            host,
            "nonce-roll",
            &[0x60; 32],
        )
        .expect("a seeded fact writes");
    }
}
