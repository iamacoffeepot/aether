//! The drain → land core of the land reactor, over a real `SqliteStore` and a
//! fake-GitHub-backed landing face — the network side the running capability
//! drives, without the mail harness. `init` / the timer / the ctx send are the
//! thin glue the chassis-boot test and compilation cover; this pins the loop that
//! turns a land decision into a landing proposal, merges that proposal once
//! the structural gates hold, and admits the `Fact::Land` it then observes.

use std::sync::Arc;

use aether_bloomery::{
    Adjudication, Admit, BloomId, Correspondence, Digest, Disposition, Event, Fact, IdempotencyKey, LandPayload,
    SourceReplicaPayload, Topic,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{
    GitObjectId, GitSource, GithubLanding, LandingSource, MainlineRef, PullRequestApi, short_hex, to_hex,
};
use aether_data::wire::{from_bytes, to_vec};

use super::{drain_and_land, drain_and_land_emitting};
use crate::bloomery::outbox::TopicOutbox;
use crate::store::{AppendOutcome, JournalWrite, SqliteStore, StoreBackend};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

// A fake-GitHub-backed source shell with the land gate set explicitly, so a
// test drives the same shell the running reactor holds.
fn shell(fake: FakeGithub, cas_land_enabled: bool) -> Arc<dyn LandingSource> {
    Arc::new(GithubLanding::new(GitSource::new(fake.clone(), Arc::new(fake), cas_land_enabled, MainlineRef::default())))
}

// Seed a fake with a base commit and a mainline ref at it, returning the fake and
// the base commit digest — the sealed base a resolved bloom lands on.
fn seeded() -> (FakeGithub, Digest) {
    let fake = FakeGithub::new();
    let base = fake.seed_base_commit(&digest(10));
    fake.seed_ref_at("heads/main", &base);
    (fake, base)
}

// Enqueue one land decision on the land topic (the bytes the reducer's
// `DispatchLand` projection would enqueue), returning its outbox sequence.
fn enqueue_land(store: &mut SqliteStore, bloom: BloomId, expected_base: Digest, new_head: Digest) -> u64 {
    let payload = LandPayload { bloom: bloom.0, expected_base, new_head };
    store.enqueue_topic(Topic::Land, &to_vec(&payload).unwrap()).unwrap()
}

#[test]
fn an_accepted_proposal_admits_a_fact_land_carrying_the_merge_commit() {
    let (fake, base) = seeded();
    let new_head = digest(90);
    // Seed the proposed head's git-object correspondence so the landing branch
    // resolves its target.
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    enqueue_land(&mut store, bloom, base, new_head);

    // An operator squash-merges the proposal the reactor opened, before the
    // reactor's own accept fires. Opening first, then merging by hand, is
    // what keeps this the observation path rather than the reactor's merge.
    let aether_bloomery_github::ProposalOutcome::Proposed { number } =
        source.land_proposal(&bloom, &base, &new_head, None).unwrap()
    else {
        panic!("expected Proposed");
    };
    let squashed = "5c".repeat(20);
    fake.merge_pull_request(number, &squashed);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert_eq!(admits.len(), 1, "an accepted proposal admits one fact");
    assert_eq!(ack_through, None, "observation is not acknowledgement; the journal is");
    let event = from_bytes::<Event>(&admits[0].event).unwrap();
    match event.fact {
        Fact::Land { bloom: landed, new_head: head } => {
            assert_eq!(landed, bloom, "the admitted land names the resolved bloom");
            // Tripwire: the admitted head is what mainline *became*. Carrying the
            // proposed head instead would record a mainline commit that a squash
            // accept never produced, and the next bloom would seal on it.
            assert_ne!(head, new_head, "the admitted head is the merge commit, not the proposed head");
            assert_eq!(
                fake.resolve_backend_object(&head)
                    .unwrap()
                    .map(GitObjectId::try_from)
                    .transpose()
                    .unwrap()
                    .map(|object| object.to_hex()),
                Some(squashed),
                "the admitted head resolves to the commit mainline actually became",
            );
        }
        other => panic!("expected Fact::Land, got {other:?}"),
    }
}

#[test]
fn a_declined_proposal_admits_nothing_and_acks_the_definitive_refusal() {
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    let aether_bloomery_github::ProposalOutcome::Proposed { number } =
        source.land_proposal(&bloom, &base, &new_head, None).unwrap()
    else {
        panic!("expected Proposed");
    };
    fake.close_pull_request(number);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert!(admits.is_empty(), "a declined proposal admits no land");
    assert_eq!(ack_through, Some(sequence), "the definitive refusal is acked rather than re-driven forever");
}

#[test]
fn base_moved_declines_to_land_but_acks_the_definitive_refusal() {
    let (fake, _base) = seeded();
    let source = shell(fake, true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    // A sealed base that no longer matches the mainline — a moved head.
    let stale_base = digest(99);
    let sequence = enqueue_land(&mut store, bloom, stale_base, digest(90));

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    // No proposal: a moved mainline forces supersession, never a land onto the
    // new head. The bloom stays supersedable; the refusal is definitive, so it is
    // acked rather than re-driven forever.
    assert!(admits.is_empty(), "a moved base admits no land");
    assert_eq!(ack_through, Some(sequence), "the definitive base-moved refusal is acked");
}

// Register `workpiece` as a member of `bloom`, optionally with the commit
// message its resolving candidate was captured under — the two rows the landing
// assembly reads (the work-order roster, and the message the lane filed).
fn seed_member(store: &mut SqliteStore, bloom: BloomId, workpiece: &str, message: Option<&str>) {
    store.record_dispatch_description(bloom.0.as_bytes(), workpiece, "the work order").unwrap();
    if let Some(message) = message {
        store.record_candidate_commit_message(bloom.0.as_bytes(), workpiece, message).unwrap();
    }
}

// The title and body the proposal for `bloom` was opened with.
fn proposal_of(fake: &FakeGithub, bloom: BloomId) -> (String, String) {
    let number = fake
        .find_pull_request_for_head(&format!("bloom/{}/landing", short_hex(&bloom.0)))
        .unwrap()
        .expect("the drain proposed one")
        .number;
    fake.pull_request_proposal(number).expect("the proposal carries its prose")
}

#[test]
fn a_single_member_lands_under_the_message_its_lane_wrote() {
    // Acceptance 1. Mainline squash-merges with the proposal's title as the
    // commit subject, so this is the whole point of the arc: the model that made
    // the change names the commit, and the body carries its prose, its closing
    // line, and the provenance the machine side never reads.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(
        &mut store,
        bloom,
        "issue-4242",
        Some("feat(crate:aether-text): shelf-pack the glyph atlas\n\nGlyphs arrive one at a time."),
    );
    enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();

    let (title, body) = proposal_of(&fake, bloom);
    assert_eq!(title, "feat(crate:aether-text): shelf-pack the glyph atlas", "the lane's own subject is the title");
    assert!(body.starts_with("Glyphs arrive one at a time."), "the message's prose leads the body: {body}");
    assert!(body.contains("\nCloses #4242"), "the member's issue closes on merge: {body}");
    assert!(body.contains(&format!("sealed base: `{}`", to_hex(&base))), "the provenance footer survives: {body}");
    assert!(body.contains("resolved head:"), "the footer keeps both ends of the swap: {body}");
}

#[test]
fn an_unusable_subject_falls_back_to_the_issue_title_and_still_lands() {
    // Acceptance 2, first rung. `Lint title` is a required check, so a subject it
    // would refuse cannot be the title — but refusing to land over it would strand
    // a bloom that has already resolved, which is why this falls back rather than
    // failing.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    fake.seed_issue_with_title(4242, "fix(crate:aether-fs): reject a traversing path", "the order");
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(&mut store, bloom, "issue-4242", Some("Rewrote the path joining\n\nIt was wrong."));
    enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();

    let (title, body) = proposal_of(&fake, bloom);
    assert_eq!(title, "fix(crate:aether-fs): reject a traversing path", "the member's source issue title stands in");
    assert!(body.contains("It was wrong."), "the unusable subject costs the title, never the prose: {body}");
    assert!(body.contains("Closes #4242"));
}

#[test]
fn a_bloom_with_nothing_to_name_it_lands_under_the_floor() {
    // Acceptance 2, last rung: no message, and an issue whose title the gate would
    // refuse just as surely. The floor is a lint-valid title by construction, so
    // the landing proceeds.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    fake.seed_issue_with_title(4242, "Rework the atlas", "the order");
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(&mut store, bloom, "issue-4242", None);
    enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();

    let (title, body) = proposal_of(&fake, bloom);
    assert_eq!(title, format!("chore(meta): land bloom {}", short_hex(&bloom.0)), "the floor title stands");
    assert!(body.contains("Closes #4242"), "a member with no message still closes its issue: {body}");
    assert!(body.contains("sealed base:"), "the provenance footer is the whole body here: {body}");
}

#[test]
fn several_members_each_get_a_section_and_the_bloom_lands_under_the_floor() {
    // Several changes are not one change, so no member's subject may stand for
    // the mainline commit — but every member's prose and closing line belongs in
    // the body. A workpiece that addresses no object contributes no closing line:
    // guessing a number would close somebody else's issue.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(&mut store, bloom, "issue-11", Some("fix(crate:aether-fs): reject a traversal\n\nThe join escaped."));
    seed_member(&mut store, bloom, "local-spike", Some("docs(guide): describe the atlas\n\nThe recipe was silent."));
    enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();

    let (title, body) = proposal_of(&fake, bloom);
    assert_eq!(title, format!("chore(meta): land bloom {}", short_hex(&bloom.0)), "no one member names the whole");
    assert!(body.contains("### fix(crate:aether-fs): reject a traversal"), "each member gets a section: {body}");
    assert!(body.contains("### docs(guide): describe the atlas"), "each member gets a section: {body}");
    assert!(body.contains("The join escaped."), "each section carries its prose: {body}");
    assert!(body.contains("Closes #11"), "the addressing member closes its issue: {body}");
    assert_eq!(body.matches("Closes #").count(), 1, "the unaddressable workpiece contributes no closing line: {body}");
}

#[test]
fn an_open_proposal_does_not_close_member_issues() {
    // Closing at propose time would mark the work done before anyone accepted
    // the landing, and a later decline would leave a closed issue pointing at
    // work that never reached the day branch.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    fake.seed_issue(11, "still in flight");
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(&mut store, bloom, "issue-11", Some("fix(crate:aether-fs): reject a traversal"));
    enqueue_land(&mut store, bloom, base, new_head);

    // Propose without accepting — closing belongs to the merge, not the
    // open. A drain would accept immediately (#5110), so this calls `land`
    // itself.
    let aether_bloomery_github::ProposalOutcome::Proposed { .. } =
        source.land_proposal(&bloom, &base, &new_head, None).unwrap()
    else {
        panic!("expected Proposed");
    };

    assert_eq!(fake.issue_is_closed(11), Some(false), "an unaccepted land must not close the source issue");
    assert!(fake.comments_on(11).is_empty(), "an unaccepted land leaves no close comment");
}

#[test]
fn landing_does_not_close_human_source_issues() {
    // After ADR-0199, GitHub issues Bloomery did not create stay human objects.
    // Land marks the local commission landed and leaves close to the replica
    // projector; treating `issue-11` as a close target would write a title
    // Bloomery does not own.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    fake.seed_issue(11, "the addressing member");
    fake.seed_issue(42, "not in this bloom");
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(&mut store, bloom, "issue-11", Some("fix(crate:aether-fs): reject a traversal\n\nThe join escaped."));
    seed_member(&mut store, bloom, "local-spike", Some("docs(guide): describe the atlas\n\nThe recipe was silent."));
    seed_member(&mut store, bloom, "issue-9999", Some("chore(meta): tidy the leftover\n\nNothing to close."));
    enqueue_land(&mut store, bloom, base, new_head);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert_eq!(admits.len(), 1, "leaving human issues open does not block the land");
    assert_eq!(ack_through, None, "the journal is still the receipt oracle");

    assert_eq!(fake.issue_is_closed(11), Some(false), "a human-authored member issue is not closed");
    assert!(fake.comments_on(11).is_empty(), "a human-authored member issue is not commented by land");
    assert_eq!(fake.issue_is_closed(42), Some(false), "an issue that is not a member is left alone");
    assert_eq!(fake.issue_is_closed(9999), None, "a workpiece naming no object does not fabricate one");
}

#[test]
fn landing_marks_member_commissions_landed_before_any_replica_close() {
    // Local status is the authority. A land that closed GitHub first and then
    // failed to stamp the commission would leave the replica open as the
    // record of a landed workpiece.
    use crate::store::CommissionBackend;
    use aether_bloomery::{CommissionStatus, Observation, Provenance, Statement, WorkpieceId};

    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake, true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    store
        .create(
            &WorkpieceId("wp-1".to_owned()),
            &Statement {
                words: b"intent".to_vec(),
                provenance: Provenance::ObservationAttestation(Observation { source: "test".to_owned() }),
                parents: Vec::new(),
            },
        )
        .unwrap();
    seed_member(&mut store, bloom, "wp-1", Some("fix(crate): land the commission"));
    enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();

    let view = store.load(&WorkpieceId("wp-1".to_owned())).unwrap().expect("commission remains");
    assert_eq!(view.head.status, CommissionStatus::Landed, "land stamps the local commission first");
    let queued = store.drain_topic(Topic::Commission).unwrap();
    assert!(queued.len() >= 2, "create and land each enqueue a replica projection");
    let last: aether_bloomery::CommissionProjection =
        from_bytes(&queued.last().unwrap().payload).expect("landed projection");
    assert_eq!(last.status, "landed");
}

// The number of the proposal the drain opened for `bloom`.
fn proposal_number(fake: &FakeGithub, bloom: BloomId) -> u64 {
    fake.find_pull_request_for_head(&format!("bloom/{}/landing", short_hex(&bloom.0)))
        .unwrap()
        .expect("the drain proposed one")
        .number
}

#[test]
fn the_reactor_merges_its_own_proposal_and_admits_the_land() {
    // Acceptance 1 (#4953, #5110). The middle step used to wait on a green
    // GitHub gate. The reactor now proposes and accepts in one pass: bloomery's
    // own gates already proved the head. Nothing in this scenario presses the
    // button, and the bloom still lands.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    enqueue_land(&mut store, bloom, base, new_head);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    let number = proposal_number(&fake, bloom);

    assert!(fake.pull_request_merged(number).unwrap(), "the reactor merged the proposal it opened");
    assert_eq!(ack_through, None, "the merge is observed; acknowledgement waits on the journal");
    assert_eq!(admits.len(), 1, "the merge and its observation settle in one pass");
    match from_bytes::<Event>(&admits[0].event).unwrap().fact {
        Fact::Land { bloom: landed, new_head: head } => {
            assert_eq!(landed, bloom);
            // Tripwire: even when the reactor merged it itself, the admitted
            // head is what mainline *became*. A squash produces a commit that is
            // not the proposed head, and deriving it from the payload rather
            // than the receipt would seal the next bloom on a commit that is on
            // no branch.
            assert_ne!(head, new_head, "the admitted head is the squash commit, not the proposed head");
        }
        other => panic!("expected Fact::Land, got {other:?}"),
    }
}

#[test]
fn a_landing_whose_proposal_drifted_is_refused_and_journaled_rather_than_merged() {
    // Acceptance 2. Every gate upstream judged the head the bloom resolved on,
    // so a proposal that has since gained a commit is proving nothing about
    // what a merge would land. The refusal is the existing landing-blocked
    // vocabulary — a `LandingRejected` the outward view renders — rather than a
    // silent sit or a retry of a decision that cannot change.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    let aether_bloomery_github::ProposalOutcome::Proposed { number } =
        source.land_proposal(&bloom, &base, &new_head, None).unwrap()
    else {
        panic!("expected Proposed");
    };
    let pushed = "ee".repeat(20);
    fake.push_to_pull_request(number, &pushed);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    assert_eq!(fake.pull_request_merged(number), Some(false), "a drifted proposal is not merged");
    assert_eq!(ack_through, Some(sequence), "the refusal is definitive, so it is acked rather than re-driven");
    assert_eq!(admits.len(), 1, "the refusal is journaled, not swallowed");
    match from_bytes::<Event>(&admits[0].event).unwrap().fact {
        Fact::LandingRejected { bloom: refused, evidence } => {
            assert_eq!(refused, bloom);
            assert_eq!(evidence.subject, new_head, "the rejection binds the head that was proposed");
        }
        other => panic!("expected Fact::LandingRejected, got {other:?}"),
    }
    // The findings the repair dispatch reads, on the bloom-scope row a failing
    // aggregate verdict uses — a refusal an operator has to read the host log
    // to understand is the invisible wait in another costume.
    let findings = store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap().expect("the refusal left findings");
    assert!(findings.contains(&pushed), "the findings name the head that was found: {findings}");
}

// The single landing-rejection admit a drain produced, or panic.
fn rejection_admit(admits: &[Admit]) -> Event {
    assert_eq!(admits.len(), 1, "expected one rejection admit, got {}", admits.len());
    from_bytes::<Event>(&admits[0].event).unwrap()
}

#[test]
fn two_landings_refused_for_the_same_cause_admit_under_distinct_keys() {
    // #5106: keyed only by bloom and cause, a second landing of the same bloom
    // that fails the same way reduces to a duplicate. The land entry is acked
    // and the reducer never learns the second refusal happened — a Resolved
    // bloom whose landing is silently gone. The two halves are one invariant
    // (the sibling of #4722): the key separates attempts, without weakening
    // the crash-replay dedup it exists for.
    //
    // Check-failure is no longer a landing refusal (#5110). Drift is: a
    // commit nobody proved arriving on the proposal the reactor opened.
    let (fake, base) = seeded();
    let (first_head, second_head) = (digest(90), digest(91));
    fake.seed_git_object(&first_head);
    fake.seed_git_object(&second_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));

    enqueue_land(&mut store, bloom, base, first_head);
    let aether_bloomery_github::ProposalOutcome::Proposed { number } =
        source.land_proposal(&bloom, &base, &first_head, None).unwrap()
    else {
        panic!("expected Proposed");
    };
    fake.push_to_pull_request(number, &"ee".repeat(20));
    let (first_lap, ack) = drain_and_land(&mut store, &source).unwrap();
    let first = rejection_admit(&first_lap);
    store.ack_topic(Topic::Land, ack.expect("the first refusal is acked")).unwrap();

    // A new outbox entry for the re-resolved bloom. The open proposal is
    // adopted; its recorded head is still the drifted sha, so the second
    // accept refuses the same way against the newer proven head.
    enqueue_land(&mut store, bloom, base, second_head);
    let (second_lap, _) = drain_and_land(&mut store, &source).unwrap();
    let second = rejection_admit(&second_lap);

    let (replayed, _) = drain_and_land(&mut store, &source).unwrap();
    let replayed = rejection_admit(&replayed);

    match (&first.fact, &second.fact) {
        (
            Fact::LandingRejected { evidence: first_evidence, .. },
            Fact::LandingRejected { evidence: second_evidence, .. },
        ) => {
            assert_eq!(first_evidence.subject, first_head, "the first refusal binds the head it judged");
            assert_eq!(second_evidence.subject, second_head, "the second refusal binds the newer head");
        }
        other => panic!("expected two LandingRejected facts, got {other:?}"),
    }
    assert_ne!(
        first.idempotency_key, second.idempotency_key,
        "two landing attempts of one bloom are two facts, even when they fail the same way",
    );
    assert_eq!(
        replayed.idempotency_key, second.idempotency_key,
        "a re-drain of the same entry still reduces to that attempt's single key",
    );
}

#[test]
fn a_base_that_moved_under_an_open_proposal_leaves_the_bloom_supersedable() {
    // A moved mainline forces supersession, never a land onto the new head
    // (ADR-0149 §The bloom) — and that has to hold at the merge as much as at
    // the propose, because the base check `land` runs deliberately stops
    // re-deciding once a proposal is open. So this refusal is *not* a
    // `LandingRejected`: the bloom stays Resolved and supersedable, exactly
    // where a base that moved before the proposal opened leaves it.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    let aether_bloomery_github::ProposalOutcome::Proposed { number } =
        source.land_proposal(&bloom, &base, &new_head, None).unwrap()
    else {
        panic!("expected Proposed");
    };
    // Mainline moves between the proposal opening and the accept.
    fake.seed_ref_at("heads/main", &digest(77));
    fake.seed_git_object(&digest(77));

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    assert_eq!(fake.pull_request_merged(number), Some(false), "a moved base is not landed onto");
    assert!(admits.is_empty(), "a moved base admits nothing; supersession is a caller act");
    assert_eq!(ack_through, Some(sequence), "the definitive refusal is acked");
}

#[test]
fn a_gated_off_land_is_a_transient_fault_that_re_drains() {
    let (fake, base) = seeded();
    // The kill switch: the land gate off makes `land` refuse with a transport
    // fault, so the entry stays unacked to re-drive when the gate is re-enabled.
    let source = shell(fake, false);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_land(&mut store, BloomId(digest(1)), base, digest(90));

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    assert!(admits.is_empty(), "a gated-off land admits nothing");
    assert_eq!(ack_through, None, "the gated entry is not acked; it re-drains");
    let _ = sequence;
}

// Journal one operator adjudication of `bloom`, the way the admit path does —
// the row the proposal assembly reads the operator's own words out of.
fn journal_adjudication(store: &mut SqliteStore, bloom: BloomId, reason: &str, disposition: Disposition) {
    let event = Event {
        idempotency_key: IdempotencyKey(format!("adj-{reason}")),
        fact: Fact::OperatorAdjudication {
            bloom,
            adjudication: Adjudication {
                findings: vec![digest(70)],
                disposition,
                reason: reason.to_owned(),
                operator: "iamacoffeepot".to_owned(),
            },
        },
    };
    let bytes = to_vec(&event).unwrap();
    let write = JournalWrite {
        idempotency_key: &event.idempotency_key.0,
        event: &bytes,
        decisions: b"decided",
        decider: "test",
    };
    store.append_event(&write).unwrap();
}

#[test]
fn an_adjudicated_bloom_lands_naming_what_was_waived_and_why() {
    // Tripwire (#4957): a landing an operator overrode must say so in the
    // proposal that becomes the mainline commit. Without this the merged history
    // reads, forever after, as a landing that passed its gates — the override is
    // known only to the coordinator that ran it, which is exactly the side
    // channel the journal-first design exists to avoid.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(&mut store, bloom, "issue-4242", Some("feat(crate:aether-text): shelf-pack the glyph atlas\n\nprose."));
    journal_adjudication(&mut store, bloom, "the fixture nit is filed forward", Disposition::Deferred { issue: 4958 });
    // A second bloom's adjudication is not this bloom's: the scan binds on the
    // bloom the fact names, so a coordinator running many blooms does not quote
    // one landing's waiver into another's.
    journal_adjudication(&mut store, BloomId(digest(2)), "somebody else's waiver", Disposition::Accepted);
    enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();

    let (_, body) = proposal_of(&fake, bloom);
    assert!(body.contains("### Adjudicated findings"), "the waiver has its own section: {body}");
    assert!(body.contains("the fixture nit is filed forward"), "in the operator's own words: {body}");
    assert!(body.contains("iamacoffeepot"), "naming who decided: {body}");
    assert!(body.contains("deferred to #4958"), "and where the finding went: {body}");
    assert!(!body.contains("somebody else's waiver"), "another bloom's waiver stays out of this body: {body}");
    assert!(body.contains("\nCloses #4242"), "the closing lines still come last: {body}");
}

#[test]
fn an_unadjudicated_bloom_lands_with_no_waiver_section() {
    // The other half of the tripwire: the section appears only when there is
    // something to say. An empty "Adjudicated findings" heading on every landing
    // would train a reader to skip the one place an override is announced.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    seed_member(&mut store, bloom, "issue-4242", Some("feat(crate:aether-text): shelf-pack the glyph atlas\n\nprose."));
    enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();

    let (_, body) = proposal_of(&fake, bloom);
    assert!(!body.contains("Adjudicated findings"), "no override, no section: {body}");
}

// The single land admit a drain produced, or panic. The land reactor's view of
// a completed merge is this event — `Fact::Land` under the deterministic key.
fn land_admit(admits: &[Admit]) -> Event {
    assert_eq!(admits.len(), 1, "expected one land admit, got {}", admits.len());
    from_bytes::<Event>(&admits[0].event).unwrap()
}

// Persist `event` under its own idempotency key — the control-core commit a
// successful Admit would have written, without going through the mail harness.
fn journal_event(store: &mut SqliteStore, event: &Event) -> AppendOutcome {
    let bytes = to_vec(event).unwrap();
    store
        .append_event(&JournalWrite {
            idempotency_key: &event.idempotency_key.0,
            event: &bytes,
            decisions: b"decided",
            decider: "test",
        })
        .unwrap()
}

#[test]
fn a_dispatch_miss_before_journal_commit_redrives_the_same_land() {
    // Tripwire: acking the Land outbox on merge observation deletes the only
    // durable redrive if the detached Admit then misses. GitHub has moved
    // mainline; the journal has not. The row must stay, and the next drain must
    // resend the same deterministic Admit without opening another proposal.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    enqueue_land(&mut store, bloom, base, new_head);
    let later = BloomId(digest(2));
    enqueue_land(&mut store, later, base, digest(91));

    let (first, ack) = drain_and_land(&mut store, &source).unwrap();
    let number = proposal_number(&fake, bloom);
    let first = land_admit(&first);
    assert_eq!(ack, None, "a miss window must not acknowledge the row");
    assert_eq!(first.idempotency_key, super::land_key(&bloom.0), "the admit is keyed by the bloom");
    match &first.fact {
        Fact::Land { bloom: landed, new_head: head } => {
            assert_eq!(*landed, bloom);
            assert_ne!(*head, new_head, "the receipt carries the merge commit, not the proposed head");
        }
        other => panic!("expected Fact::Land, got {other:?}"),
    }

    let (again, ack) = drain_and_land(&mut store, &source).unwrap();
    let again = land_admit(&again);
    assert_eq!(ack, None, "still no journal row, so still no ack");
    assert_eq!(again, first, "the redrive is the exact same land admit");
    assert_eq!(proposal_number(&fake, bloom), number, "a miss must not open a second proposal");
    assert_eq!(fake.pull_request_merged(number), Some(true), "the original merge stands");
    assert_eq!(
        fake.find_pull_request_for_head(&format!("bloom/{}/landing", short_hex(&later.0))).unwrap(),
        None,
        "an unconfirmed landing holds later rows; the next bloom is not proposed",
    );
    assert_eq!(store.drain_topic(Topic::Land).unwrap().len(), 2, "both rows stay in the unacked prefix");
}

#[test]
fn a_restart_after_journal_commit_acks_without_remerging() {
    // Tripwire: once the journal holds the land key, a coordinator restart is
    // replay plus outbox republish, not a second merge and not a second receipt.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    let (admits, ack) = drain_and_land(&mut store, &source).unwrap();
    let number = proposal_number(&fake, bloom);
    assert_eq!(ack, None);
    let event = land_admit(&admits);
    assert_eq!(journal_event(&mut store, &event), AppendOutcome::Applied(1), "one journaled landing transition");
    assert_eq!(
        journal_event(&mut store, &event),
        AppendOutcome::Duplicate,
        "the deterministic key is the only landing"
    );

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert!(admits.is_empty(), "a committed land does not fabricate another receipt");
    assert_eq!(ack_through, Some(sequence), "the restart observes the key and acknowledges");
    assert_eq!(proposal_number(&fake, bloom), number, "restart must not open a second proposal");
    assert_eq!(fake.pull_request_merged(number), Some(true), "restart must not merge again");
}

#[test]
fn journal_confirmation_acknowledges_the_land_prefix() {
    // Tripwire: a successful Admit is what earns the ack. After the journal
    // holds the key, the prefix advances, the outbox goes quiet, and nothing
    // later is eligible to dispatch more land work for this bloom.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    let (admits, ack) = drain_and_land(&mut store, &source).unwrap();
    let number = proposal_number(&fake, bloom);
    assert_eq!(ack, None, "admission has not committed");
    let event = land_admit(&admits);
    match event.fact {
        Fact::Land { bloom: landed, .. } => assert_eq!(landed, bloom, "the Landed view names this bloom"),
        other => panic!("expected Fact::Land, got {other:?}"),
    }
    journal_event(&mut store, &event);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert!(admits.is_empty(), "confirmation does not re-admit");
    assert_eq!(ack_through, Some(sequence), "confirmation acknowledges through the landed row");
    store.ack_topic(Topic::Land, ack_through.unwrap()).unwrap();
    assert!(store.drain_topic(Topic::Land).unwrap().is_empty(), "no later land work remains eligible");

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert!(admits.is_empty(), "an empty prefix admits nothing");
    assert_eq!(ack_through, None, "an empty prefix acknowledges nothing");
    assert_eq!(proposal_number(&fake, bloom), number);
    assert_eq!(fake.pull_request_merged(number), Some(true));
}

#[test]
fn a_committed_land_emits_a_source_replica_row_only_after_the_receipt_is_admitted() {
    // Host-minted: the replica topic is not part of the land commit. Before
    // the journal holds the land key there is nothing to push; after it does,
    // one source-replica row carries the admitted head.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake, true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    enqueue_land(&mut store, bloom, base, new_head);

    let (admits, _) = drain_and_land_emitting(&mut store, &source, true).unwrap();
    assert!(
        store.drain_topic(Topic::SourceReplica).unwrap().is_empty(),
        "no replica row before the receipt is admitted"
    );
    assert!(!admits.is_empty(), "the land is observed before the replica is minted");

    journal_event(&mut store, &land_admit(&admits));
    let (_, ack) = drain_and_land_emitting(&mut store, &source, true).unwrap();
    assert_eq!(ack, Some(1));

    let entries = store.drain_topic(Topic::SourceReplica).unwrap();
    assert_eq!(entries.len(), 1, "exactly one replica request after admit");
    let payload: SourceReplicaPayload = from_bytes(&entries[0].payload).unwrap();
    assert_eq!(payload.new_head, new_head, "the replica request carries the landed head the outbox named");
}
