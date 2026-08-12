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
//!
//! This is also where the scripted seam's refusal side is pinned, since a live
//! outstanding order is what a refusal has to be probed against and the happy
//! path is where one is cheapest to hold. The two refused uploads sit beside the
//! Construct verdict and leave the bloom's own progress untouched.

mod common;
pub mod fixture;

use aether_bloomery::{BloomStatus, Nonce, StudyCost, StudyRecord};
use aether_chassis_bloomery::artifacts::GetResult;
use aether_chassis_bloomery::bloomery::{ScriptedEvidenceResult, ScriptedUpload};
use aether_data::wire::from_bytes;
use fixture::{FixtureHarness, captured, digest, passed};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

/// A measured attempt cost, distinct in every column so the study record read
/// back at the end cannot be mistaken for a default-constructed one.
const fn measured_cost() -> StudyCost {
    StudyCost {
        // Zero, and not because the attempt was free: pricing is the sealed
        // table's job, and the default table prices nothing.
        cost_micro_usd: 0,
        turns: 4,
        duration_millis: 90_210,
        input_tokens: 1_100,
        cache_write_tokens: 210,
        cache_write_1h_tokens: 160,
        cache_write_5m_tokens: 50,
        cache_read_tokens: 8_100,
        output_tokens: 910,
    }
}

/// The same upload, carrying a measured cost — what a lane that ran under a
/// usage-recording harness reports alongside its verdict.
fn measured(upload: ScriptedUpload, cost: StudyCost) -> ScriptedUpload {
    ScriptedUpload { cost: Some(cost), ..upload }
}

/// Assert the broker refused an upload, and refused it for `reason` — the
/// rendered [`IntakeRefusal`] variant the scripted reply carries.
///
/// [`IntakeRefusal`]: aether_chassis_bloomery::bloomery::IntakeRefusal
fn assert_refused(result: &ScriptedEvidenceResult, reason: &str) {
    let ScriptedEvidenceResult::Refused { refusal } = result else {
        panic!("the intake boundary took an upload it must refuse ({reason}): {result:?}");
    };
    assert!(refusal.contains(reason), "expected a {reason} refusal, got {refusal}");
}

#[test]
fn a_bloom_with_all_scripted_verdicts_lands() {
    let mut harness = FixtureHarness::start("all-passing-scenario");
    let scope_revision = digest(0x51);
    let sealed_on = harness.view().mainline;
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

    // The trust boundary every scenario rests on, probed once against a live
    // order. Neither upload perturbs what follows: both refusals are decided
    // before the reducer is reached, write nothing, and leave the order live for
    // the honest verdict below.
    //
    // Tripwire: the scripted seam is a seam only while it forwards the
    // scenario's *own* nonce and subject to `admit_uploaded` unexamined. Two
    // conveniences would quietly turn it into a bypass, and each assertion pins
    // one. Filling the subject in from the stored order — it is right there, and
    // the harness has to read it anyway — would make the digest binding `x == x`
    // and admit the mismatched upload. Resolving the nonce in the handler rather
    // than routing it through the broker would admit the fabricated one. Under
    // either, every seal-to-landed assertion in this suite still passes, because
    // a scenario only ever scripts uploads that are already honest.
    let fabricated = ScriptedUpload { nonce: Nonce("nonce-that-names-no-order".to_owned()), ..passed(&construct) };
    assert_refused(&harness.upload(&fabricated), "UnknownNonce");

    let misbound = ScriptedUpload { subject: digest(0xEE), ..passed(&construct) };
    assert_refused(&harness.upload(&misbound), "DigestMismatch");

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

    assert_ne!(harness.view().mainline, sealed_on, "mainline advanced off the base the bloom sealed on");
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
