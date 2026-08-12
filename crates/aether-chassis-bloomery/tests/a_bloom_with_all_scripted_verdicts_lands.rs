//! One member, every verdict passing, seal to landed — the whole reactor chain
//! run once with nothing hand-placed between its links.
//!
//! Every outbox row this scenario consumes was written by the real control core
//! committing the real reducer's decisions, and every one is drained by the
//! boot-constructed reactor that owns its topic. That is what the reactors' own
//! unit tests cannot cover: each of those enqueues the row its upstream would
//! have produced, so a producer that emits a payload its consumer cannot act on
//! — or a stage transition that produces no next input at all — passes both
//! sides and stalls only in production.

mod common;
mod fixture;

use aether_bloomery::{BloomStatus, StudyRecord};
use aether_chassis_bloomery::artifacts::GetResult;
use aether_data::wire::from_bytes;
use fixture::{FixtureHarness, captured, digest, measured, measured_cost, passed};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

#[test]
fn a_bloom_with_all_scripted_verdicts_lands() {
    let mut harness = FixtureHarness::start("all-passing-scenario");
    let scope_revision = digest(0x51);
    let bloom = harness.seal_member(WORKPIECE, scope_revision);

    // Construct. The seal's dispatch decision is already in the outbox; the tick
    // inside `await_order` is what turns it into a submitted order the
    // coordinator waits on, displaying the member's frozen scope revision.
    let construct = harness.await_order();
    assert_eq!(
        construct.displayed_digest,
        scope_revision.as_bytes().to_vec(),
        "a member with no capture yet is ordered against its sealed scope revision",
    );

    let candidate = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xC2));
    let cost = measured_cost();
    let key = harness.upload_admitted(&measured(captured(&construct, candidate), cost));
    assert!(key.starts_with("aether.bloomery.attempt:"), "a non-terminal member stage admits as an attempt: {key}");

    // Verify. The passing Construct moved the cursor and dispatched the next
    // stage — against the capture, not the revision, which is exactly what a
    // hand-placed outbox row would have asserted into existence rather than
    // observed.
    let verify = harness.await_order();
    assert_eq!(
        verify.displayed_digest,
        candidate.tree.as_bytes().to_vec(),
        "once a member has a capture, its next order is ordered against that tree",
    );

    let key = harness.upload_admitted(&passed(&verify));
    assert!(key.starts_with("aether.bloomery.integrate:"), "a passing terminal Verify integrates directly: {key}");

    // The claim set is complete, so the reducer dispatched the git-side fold;
    // the aggregate gates and the landing follow from it.
    harness.land_the_fold(bloom);

    assert_ne!(harness.view().mainline, harness.base(), "mainline advanced off the base the bloom sealed on");
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed);

    // The study record the Construct attempt's cost produced. The executor
    // reactor filed it through its own artifacts handle, opened from the
    // coordinator config; this reads the root the *artifacts* config named. A
    // reactor that resolved a different root — a platform data dir, say — would
    // have filed a real record where nothing reads it (#4705), and the index row
    // below would name a digest this root does not hold.
    let indexed = harness
        .study_index_row(bloom, scope_revision)
        .expect("the admitted construct attempt's cost was indexed under the bloom");
    let GetResult::Ok { bytes, .. } = harness.artifact(&indexed) else {
        panic!("the study artifact the reactor filed is not at the configured artifacts root");
    };
    let record: StudyRecord = from_bytes(&bytes).expect("the study artifact decodes");
    assert!(record.grades(&scope_revision), "the study record grades the attempt the order displayed");
    assert_eq!(record.cost, cost, "the measured columns survive the round trip through the content store");
}
