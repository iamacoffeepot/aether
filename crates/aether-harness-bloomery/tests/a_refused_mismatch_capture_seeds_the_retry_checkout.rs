//! A refused `DigestMismatch` Construct capture must seed its own retry.
//!
//! Pre-fix (#6013): the refused tree reached the member-checkpoint ref while
//! the fault-driven retry checked out the sealed base — the recovery fault
//! carried no capture, and the reducer records checkpoints only from failing
//! constructs. The retry rebuilt work the refused lane had already done with
//! the pushed checkpoint sitting unreferenced beside it. Now the retry's
//! capture is a child of the refused capture: the candidate ref's parent is
//! the checkpoint, not the sealed base.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Digest, FakeKeyProvider, KeyId, StageId, signed_approval};
use aether_bloomery_github::{candidate_ref_name, member_checkpoint_ref_name};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::CommissionBackend;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

/// The member whose first construct binds the wrong subject.
const MEMBER: &str = "wp-seed";

fn approve(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval =
            signed_approval(KeyId(String::from("refused-mismatch-seeds-retry")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the authored scope retains its signed approval");
    }
}

/// The commit a ref points at, or `None` when the authority holds no such ref.
///
/// `for-each-ref` rather than `rev-parse`, because the scenario waits for the
/// ref to appear and `rev-parse` of an absent ref is a failed git command, not
/// an empty answer.
fn ref_commit(authority: &Repo, name: &str) -> Option<String> {
    let listed = authority.git(&["for-each-ref", "--format=%(objectname)", name]);
    listed.split_whitespace().next().map(str::to_owned)
}

#[test]
fn a_refused_mismatch_capture_seeds_the_retry_checkout() {
    let authority = Repo::with_formatted_example_project();
    // The first Construct run binds the wrong subject but still captures; the
    // retry occurrence falls back to the passing default.
    let script = LaneScript::all_passing().then_for(MEMBER, StageId::Construct, LaneMode::WrongSubject);
    let mut harness = HarnessBuilder::local_authority(&authority).script(&script).start("refused-mismatch-seeds-retry");
    let scope = harness.author_scope_revision(MEMBER, &["crates/example-a/**"]);
    approve(&harness, &[scope]);
    let bloom = harness.seal_member(MEMBER, scope);

    let checkpoint_ref = member_checkpoint_ref_name(&bloom, MEMBER);
    harness.pump_until("the refused construct's tree reaches the member-checkpoint ref", |_| {
        ref_commit(&authority, &checkpoint_ref).is_some()
    });
    let checkpoint = ref_commit(&authority, &checkpoint_ref).expect("the checkpoint ref resolves");

    let candidate_ref = candidate_ref_name(&bloom, MEMBER);
    harness.pump_until("the retry's capture reaches the candidate ref", |_| {
        ref_commit(&authority, &candidate_ref).is_some()
    });
    let candidate = ref_commit(&authority, &candidate_ref).expect("the candidate ref resolves");

    let parent = authority.git(&["rev-parse", &format!("{candidate}^")]);
    assert_eq!(
        parent.trim(),
        checkpoint.trim(),
        "the fault-driven retry builds on the refused capture, not the sealed base",
    );
}
