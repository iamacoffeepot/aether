//! Discriminated proof facts and the only path that writes them (ADR-0200).
//!
//! A runner report is not a fact. [`discriminate`] keeps only tests that
//! agreed across two independent runs; [`record_proof_facts`] is the verify
//! path that persists those, and it will not accept a raw report.

use std::collections::BTreeMap;

use super::ClosureKey;

#[cfg(feature = "runtime")]
use crate::store::{ProofFactWrite, StoreBackend};

/// The result a proof fact records. Append-only: a new spelling goes on the
/// end, never a rename or reorder of these two.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ProofResult {
    /// The test passed in both discriminated runs.
    Green,
    /// The test failed in both discriminated runs.
    Red,
}

impl ProofResult {
    /// The table spelling. Load-bearing: persisted rows carry this text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
        }
    }

    /// Parse a persisted row's `result` column. Anything other than the two
    /// spellings [`Self::as_str`] writes is not a fact.
    #[must_use]
    pub fn from_stored(stored: &str) -> Option<Self> {
        match stored {
            "green" => Some(Self::Green),
            "red" => Some(Self::Red),
            _ => None,
        }
    }
}

/// Per-test outcomes from one runner invocation. Not a fact — a single run
/// can be a flake.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunnerReport {
    outcomes: BTreeMap<String, ProofResult>,
}

impl RunnerReport {
    /// An empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one test's outcome in this run. A later call for the same id
    /// overwrites — one run has one result per test.
    pub fn insert(&mut self, test_id: impl Into<String>, result: ProofResult) {
        self.outcomes.insert(test_id.into(), result);
    }
}

/// One test that agreed across both runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiscriminatedFact {
    /// The test the two runs agreed on.
    pub test_id: String,
    /// The result both runs reported.
    pub result: ProofResult,
}

/// Facts that have passed flake discrimination (ADR-0200 integrity rule 1).
///
/// The only constructor is [`discriminate`]. A [`RunnerReport`] cannot be
/// recorded, so a single-run observation has no path into the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscriminatedFacts {
    facts: Vec<DiscriminatedFact>,
}

impl DiscriminatedFacts {
    /// The facts in test-id order.
    pub fn iter(&self) -> impl Iterator<Item = &DiscriminatedFact> {
        self.facts.iter()
    }

    /// How many facts discrimination accepted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// Whether discrimination accepted nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
}

/// Which verify position is recording, and whose closure keys it stamps.
pub enum ProofSource<'a> {
    /// Member verify: one closure, the member's own.
    Member {
        /// The member package's closure key.
        closure: ClosureKey,
    },
    /// Aggregate verify: every member closure the weave just proved.
    Aggregate {
        /// One key per member the aggregate run covers.
        closures: &'a [ClosureKey],
    },
    /// Daily sweep: one invalidated closure the idle prover just converted.
    Sweep {
        /// The post-land closure key the unknown fact was addressed by.
        closure: ClosureKey,
    },
}

/// Keep only tests that appear in both reports with the same result.
///
/// A test present in only one run, or present in both with different
/// results, is a flake and is dropped. Two independent runs are the
/// discrimination; passing the same report twice is the caller's lie.
#[must_use]
pub fn discriminate(first: &RunnerReport, second: &RunnerReport) -> DiscriminatedFacts {
    let facts = first
        .outcomes
        .iter()
        .filter(|&(test_id, result)| second.outcomes.get(test_id) == Some(result))
        .map(|(test_id, result)| DiscriminatedFact { test_id: test_id.clone(), result: *result })
        .collect();
    DiscriminatedFacts { facts }
}

/// Record already-discriminated facts from the verify path.
///
/// Member verify stamps its one closure key. Aggregate verify stamps every
/// member key it proves, so each member's address holds the same fact.
/// Nothing here accepts a [`RunnerReport`].
///
/// # Errors
/// The store write failed.
#[cfg(feature = "runtime")]
pub fn record_proof_facts(
    store: &mut dyn StoreBackend,
    source: &ProofSource<'_>,
    facts: &DiscriminatedFacts,
    host_class: &super::HostClass,
    producing_dispatch: &str,
    producing_bloom: &[u8],
) -> rusqlite::Result<usize> {
    use std::slice::from_ref;

    let closures: &[ClosureKey] = match source {
        ProofSource::Member { closure } | ProofSource::Sweep { closure } => from_ref(closure),
        ProofSource::Aggregate { closures } => closures,
    };
    let writes: Vec<ProofFactWrite<'_>> = closures
        .iter()
        .flat_map(|closure| {
            facts.iter().map(|fact| ProofFactWrite {
                closure_key: closure.as_bytes().as_slice(),
                test_id: fact.test_id.as_str(),
                result: fact.result.as_str(),
                host_class: host_class.as_str(),
                producing_dispatch,
                producing_bloom,
            })
        })
        .collect();
    let written = writes.len();
    store.append_proof_facts(&writes)?;
    Ok(written)
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use aether_bloomery::Digest;

    use super::{ProofResult, ProofSource, RunnerReport, discriminate, record_proof_facts};
    use crate::bloomery::verify::{ClosureKey, HostClass};
    use crate::store::{SqliteStore, StoreBackend};

    #[test]
    fn an_undiscriminated_single_run_never_reaches_the_table_and_a_discriminated_green_lands_once() {
        // ADR-0200 integrity rule 1: a flake that only one run saw must not
        // become a fact, and a green that both runs agreed on lands exactly
        // one row. The type system already refuses a raw RunnerReport; this
        // exercises the discriminate-then-record path the verify positions
        // share.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let closure = ClosureKey::from_digest(Digest::from_bytes([0xA1; 32]));
        let host = HostClass::new("fleet");
        let bloom = [0xB1; 32];

        let mut first = RunnerReport::new();
        first.insert("crate::flaky", ProofResult::Green);
        let mut second = RunnerReport::new();
        second.insert("crate::flaky", ProofResult::Red);
        let written = record_proof_facts(
            &mut store,
            &ProofSource::Member { closure },
            &discriminate(&first, &second),
            &host,
            "nonce-flake",
            &bloom,
        )
        .expect("a dropped flake still writes nothing");
        assert_eq!(written, 0, "discrimination accepted no fact");
        assert!(
            store.list_proof_facts().expect("the table reads").is_empty(),
            "an undiscriminated single-run flake must not reach the table"
        );

        let mut green_first = RunnerReport::new();
        green_first.insert("crate::stable", ProofResult::Green);
        let mut green_second = RunnerReport::new();
        green_second.insert("crate::stable", ProofResult::Green);
        let written = record_proof_facts(
            &mut store,
            &ProofSource::Member { closure },
            &discriminate(&green_first, &green_second),
            &host,
            "nonce-green",
            &bloom,
        )
        .expect("a discriminated green writes");
        assert_eq!(written, 1, "one fact, one member key");

        let rows = store.list_proof_facts().expect("the table reads");
        assert_eq!(rows.len(), 1, "a discriminated green lands exactly one row");
        assert_eq!(rows[0].closure_key, closure.as_bytes());
        assert_eq!(rows[0].test_id, "crate::stable");
        assert_eq!(rows[0].result, "green");
        assert_eq!(rows[0].host_class, "fleet");
        assert_eq!(rows[0].producing_dispatch, "nonce-green");
        assert_eq!(rows[0].producing_bloom, bloom);
    }

    #[test]
    fn an_aggregate_run_stamps_every_member_closure_it_proves() {
        // The weave proves one tree; each member's address has to hold the
        // fact or consultation at a member key would miss what the aggregate
        // already paid for.
        let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
        let keys = [
            ClosureKey::from_digest(Digest::from_bytes([0x01; 32])),
            ClosureKey::from_digest(Digest::from_bytes([0x02; 32])),
        ];
        let mut first = RunnerReport::new();
        first.insert("crate::shared", ProofResult::Green);
        let mut second = RunnerReport::new();
        second.insert("crate::shared", ProofResult::Green);

        let written = record_proof_facts(
            &mut store,
            &ProofSource::Aggregate { closures: &keys },
            &discriminate(&first, &second),
            &HostClass::new("fleet"),
            "nonce-agg",
            &[0xB2; 32],
        )
        .expect("the aggregate writes");
        assert_eq!(written, 2);

        let rows = store.list_proof_facts().expect("the table reads");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].closure_key, keys[0].as_bytes());
        assert_eq!(rows[1].closure_key, keys[1].as_bytes());
        assert!(rows.iter().all(|row| row.test_id == "crate::shared" && row.result == "green"));
    }
}
