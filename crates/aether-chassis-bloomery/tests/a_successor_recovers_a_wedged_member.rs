//! A member that exhausts its Construct budget wedges, and a superseding bloom
//! at a changed scope revision takes the workpiece back onto the line and lands
//! it.
//!
//! A wedge is terminal by design: the member stops dispatching and its bloom can
//! never resolve. Supersession is the only route back, and it is a route that
//! crosses every seam at once — the reducer releases the predecessor's claim,
//! admits the successor's, and decides a fresh entry-stage dispatch, all in one
//! decision set the control core commits and the executor reactor then has to
//! drain. A supersession that released and claimed but never reached the
//! executor leaves the recovered workpiece exactly as stuck as the wedge did,
//! and the projection would show a healthy successor the whole time.
//!
//! The changed scope revision is what makes it a recovery rather than an
//! inheritance: a successor re-admitting the same workpiece at the same revision
//! inherits the predecessor's claim and folds its work, which is the wrong
//! answer for a member whose work never passed.

use aether_bloomery::{BloomId, BloomStatus, Digest, Fact, Outcome, StageId};
use aether_chassis_bloomery::bloomery::{ScriptedUpload, ScriptedVerdict};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_harness_bloomery::{FixtureHarness, captured, digest, draft, member, passed, verdict};

/// The workpiece the wedged member covers, and the one the successor recovers.
const WORKPIECE: &str = "wp";

/// A failing verdict for a stage that names no verifiers — every stage but the
/// terminal member Verify. It captures nothing: a failing attempt's capture is
/// discarded by the reducer, so uploading one would assert nothing.
fn failed(order: &OutstandingOrder) -> ScriptedUpload {
    verdict(order, ScriptedVerdict::VerificationFailed)
}

/// Supersede `predecessor` with a single-member successor at `scope_revision`,
/// through the same `Admit` door every external caller uses.
///
/// Built here rather than in the harness because supersession is what this
/// scenario is about, and the spec it needs is three public calls: the base the
/// coordinator will accept, the member, and the sealed draft.
fn supersede(harness: &mut FixtureHarness, predecessor: BloomId, scope_revision: Digest) -> BloomId {
    let successor = draft(harness.view().mainline, &[member(WORKPIECE, scope_revision)]);
    let id = successor.id();

    match harness.admit("recover-the-wedged-member", Fact::Supersede { predecessor, successor }) {
        Outcome::Superseded { .. } => id,
        other => panic!("the recovery must supersede: {other:?}"),
    }
}

#[test]
fn a_successor_recovers_a_wedged_member() {
    let mut harness = FixtureHarness::start("wedge-recovery-scenario");
    let wedged = harness.seal_member(WORKPIECE, digest(0x51));

    // Two failing Construct attempts. The sealed catalog allows two, so the
    // first re-dispatches the stage and the second exhausts it.
    let first = harness.await_order();
    harness.upload_admitted(&failed(&first));

    let retry = harness.await_order();
    assert_ne!(retry.nonce, first.nonce, "a failing attempt inside its budget is re-dispatched as a fresh attempt");
    harness.upload_admitted(&failed(&retry));

    harness.dispatch_tick();
    assert!(harness.orders().is_empty(), "an exhausted stage stops dispatching rather than looping");
    let wedge = harness.bloom(wedged).members[0].wedge.expect("the exhausted member records why it stopped");
    assert_eq!(wedge.stage, StageId::Construct, "the wedge names the stage whose budget ran out");

    // The recovery. A changed scope revision means the successor re-admits the
    // workpiece rather than inheriting a claim the member never earned.
    let successor = supersede(&mut harness, wedged, digest(0x52));
    let predecessor = harness.bloom(wedged);
    assert_eq!(predecessor.status, BloomStatus::Superseded, "the wedged bloom is retired by its successor");
    assert_eq!(predecessor.superseded_by, Some(successor), "and names the successor that replaced it");

    // The successor's member enters the line from the top — a workpiece the
    // reducer had stopped dispatching is dispatching again.
    let construct = harness.await_order();
    assert_eq!(
        construct.bloom,
        successor.0.as_bytes().to_vec(),
        "the fresh dispatch belongs to the successor, not the bloom it replaced",
    );
    assert!(
        harness.bloom(successor).members[0].wedge.is_none(),
        "the successor's member carries none of the predecessor's terminal state",
    );

    let candidate = harness.seed_capture(successor, WORKPIECE, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    harness.land_the_fold(successor);

    assert_eq!(
        harness.bloom(successor).status,
        BloomStatus::Landed,
        "the recovered workpiece lands under the successor"
    );
    assert_eq!(harness.bloom(wedged).status, BloomStatus::Superseded, "and the wedged predecessor stays retired");
}
