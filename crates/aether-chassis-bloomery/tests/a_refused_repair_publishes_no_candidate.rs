#![cfg(all(unix, feature = "github"))]

//! A repair the reducer refuses must spend no candidate-ref publication
//! (issue #5560, ADR-0181).
//!
//! The repair door takes two host effects on a `from_commit` body: it records
//! the derived correspondence rows, and it force-pushes the workpiece's
//! candidate ref. The two are not alike. Correspondence is additive — rows keyed
//! by digests nothing else names, which a refused repair simply leaves unread.
//! The candidate ref is the live address the member's own checkout resolves, and
//! moving it is destructive: the member the operator aimed at may be mid-lap on
//! the candidate its last capture published, and the ref is how that capture is
//! found.
//!
//! Pre-fix both ran during body resolution, ahead of the admit — so `Held`,
//! `NotWedged`, and `AlreadyResolved`, the three refusals this door exists to
//! make, each arrived *after* the ref had already been overwritten. An operator
//! reading `422 not wedged` would reasonably believe nothing happened.
//!
//! The scenario drives the real door over HTTP against a member that is not
//! wedged, in a cell whose candidate pusher refuses every push (the fixture
//! boot's seam, #4842). That refusal is the tell: the unfixed coordinator
//! reaches the pusher before the reducer and answers with the *push's*
//! complaint, while the fixed one never reaches it and answers with the
//! reducer's own refusal.

use aether_bloomery::testing::digest;
use aether_harness_bloomery::FixtureHarness;

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

#[test]
fn a_repair_the_reducer_refuses_publishes_no_candidate_ref() {
    let mut harness = FixtureHarness::start("refused-repair-publishes-no-candidate");
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    // Nothing has run, so the member is not wedged and the reducer must refuse.
    // `HEAD` is named because the door derives against the coordinator process's
    // own repository, which under `cargo test` is this checkout — any reachable
    // commit will do, since what the scenario watches is whether the door
    // publishes before it asks.
    let (status, body) = harness.post(
        &format!("/blooms/{}/members/{WORKPIECE}/repair", bloom.0.to_hex()),
        r#"{"from_commit":"HEAD","reason":"hand the member a candidate it did not ask for","operator":"scenario"}"#,
    );

    assert_eq!(status, 422, "a refused operator door answers 422 (ADR-0181): {body}");
    assert!(
        body.contains("NotWedged"),
        "the operator must be told what the reducer refused, not what a push complained about: {body}",
    );
    assert!(
        !body.contains("refusing to push"),
        "a repair the reducer has not admitted must never reach the candidate-ref pusher: {body}",
    );
}
