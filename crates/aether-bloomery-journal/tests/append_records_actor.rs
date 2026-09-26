//! `AppendRecords`: the native bundle driver's one caused write (ADR-0226 decision 10).

mod actor_support;

use std::path::Path;
use std::sync::{Arc, mpsc};

use aether_actor::{ActorRef, ErasedActorRef};
use aether_bloomery_journal::{Entry, Journal, JournalActor, JournalReader, ReadCacheBudget, Seq};
use aether_bloomery_kinds::{
    Activated, AppendRecords, AppendRecordsResult, Detail, Digest, DriverRecord, EncodedArtifact, Head, NativeOrigin,
    OpaqueBytes, ProgramName, ProgramRef, ReactionFailed, ReactorName, RecordedHead, RecordedHeadMove, RequestSource,
    Requested, RuleName, Transition,
};
use aether_substrate::Subname;
use aether_substrate::chassis::builder::PassiveChassis;
use aether_substrate::mail::registry::{OwnedDispatch, Registry};
use aether_substrate::testing::{TestChassis, bare_substrate, boot_test_chassis_with};

use actor_support::{TestAnchor, caller, reply, request};

/// One journal actor over a fresh, empty root, observed by a reader.
struct Fixture {
    registry: Arc<Registry>,
    _chassis: PassiveChassis<TestChassis>,
    actor: ActorRef<JournalActor>,
    caller: ErasedActorRef,
    replies: mpsc::Receiver<OwnedDispatch>,
    journal: JournalReader,
}

impl Fixture {
    fn start(path: &Path) -> Self {
        let (registry, mailer) = bare_substrate();
        let (caller, replies) = caller(&registry, "test.append_records.caller");
        let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
        let actor = chassis
            .spawn_actor::<JournalActor>(
                Subname::Named("append-records"),
                ReadCacheBudget::default(),
                Journal::open(path).expect("open the journal root"),
            )
            .finish()
            .expect("birth");
        let journal = JournalReader::open(path).expect("observe journal");
        Self { registry, _chassis: chassis, actor, caller, replies, journal }
    }

    fn append(&self, correlation: u64, command: &AppendRecords) -> AppendRecordsResult {
        request(&self.registry, self.actor, self.caller, correlation, command);
        reply(&self.replies, correlation)
    }

    fn head(&self) -> Seq {
        self.journal.head().expect("journal head")
    }

    fn stored(&self, digest: &Digest) -> bool {
        self.journal.get_bytes(digest).expect("read artifact").is_some()
    }

    fn entries(&self) -> Vec<Entry> {
        self.journal.read(Seq(0), 128).expect("read events")
    }
}

fn program_ref() -> ProgramRef {
    ProgramRef::new(Digest::from_bytes([9; 32]), ProgramName::new("bloomery.test.program").expect("program name"))
}

fn native_source() -> RequestSource {
    RequestSource::Native { origin: NativeOrigin::new("test.driver").expect("native origin"), key: 1 }
}

fn reaction_source() -> RequestSource {
    RequestSource::Reaction {
        bundle: Digest::from_bytes([3; 32]),
        reactor: ReactorName::new("bloomery.test.reactor").expect("reactor name"),
        rule: RuleName::new("bloomery.test.rule").expect("rule name"),
        ordinal: 0,
    }
}

#[test]
fn stale_fence_writes_no_records_or_artifacts() {
    // Catches a handler that staged before the fence was judged, or skipped
    // the fence check entirely.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));

    let first_input = EncodedArtifact::opaque_bytes(b"first-input");
    let first_digest = first_input.digest();
    let first = Requested { program: program_ref(), input: first_digest, source: native_source() };
    let committed = fixture.append(
        1,
        &AppendRecords::new(vec![first_input], vec![DriverRecord::Requested { cause: None, record: first }], 0),
    );
    assert_eq!(committed, AppendRecordsResult::Committed { head: 1, artifacts: vec![first_digest] });
    assert_eq!(fixture.head(), Seq(1));

    let stale_input = EncodedArtifact::opaque_bytes(b"stale-input");
    let stale_digest = stale_input.digest();
    let stale = Requested { program: program_ref(), input: stale_digest, source: native_source() };
    let refused = fixture.append(
        2,
        &AppendRecords::new(vec![stale_input], vec![DriverRecord::Requested { cause: None, record: stale }], 0),
    );
    assert_eq!(refused, AppendRecordsResult::Conflict { actual: 1 });
    assert_eq!(fixture.head(), Seq(1));
    assert!(!fixture.stored(&stale_digest));
}

#[test]
fn a_cause_outside_the_fenced_prefix_is_refused() {
    // Catches a missing or off-by-one range check (`<` versus `<=`), or
    // accepting a zero cause.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));

    let seed_input = EncodedArtifact::opaque_bytes(b"seed-input");
    let seed_digest = seed_input.digest();
    let seed = Requested { program: program_ref(), input: seed_digest, source: native_source() };
    assert_eq!(
        fixture.append(
            1,
            &AppendRecords::new(vec![seed_input], vec![DriverRecord::Requested { cause: None, record: seed }], 0)
        ),
        AppendRecordsResult::Committed { head: 1, artifacts: vec![seed_digest] }
    );

    for (correlation, bad_cause) in [(2u64, 2u64), (3u64, 0u64)] {
        let sibling_input = EncodedArtifact::opaque_bytes(format!("sibling-{correlation}").as_bytes());
        let sibling_digest = sibling_input.digest();
        let sibling = Requested { program: program_ref(), input: sibling_digest, source: reaction_source() };
        let bad = ReactionFailed { bundle: Digest::from_bytes([8; 32]), reactor: None, reason: Detail::new("boom") };

        let records = vec![
            DriverRecord::Requested { cause: Some(1), record: sibling },
            DriverRecord::ReactionFailed { cause: bad_cause, record: bad },
        ];
        let refused = fixture.append(correlation, &AppendRecords::new(vec![sibling_input], records, 1));
        assert!(matches!(refused, AppendRecordsResult::Err { .. }), "cause {bad_cause} must be refused: {refused:?}");
        assert_eq!(fixture.head(), Seq(1));
        assert!(!fixture.stored(&sibling_digest));
    }
}

#[test]
fn a_transition_commits_only_with_its_input_and_result() {
    // Catches staged results committed separately from the record, a lost
    // cause, and a transition recorded over nothing.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));

    let input = EncodedArtifact::opaque_bytes(b"transition-input");
    let input_digest = input.digest();
    let requested = Requested { program: program_ref(), input: input_digest, source: native_source() };
    assert_eq!(
        fixture.append(
            1,
            &AppendRecords::new(vec![input], vec![DriverRecord::Requested { cause: None, record: requested }], 0)
        ),
        AppendRecordsResult::Committed { head: 1, artifacts: vec![input_digest] }
    );

    let result = EncodedArtifact::opaque_bytes(b"transition-result");
    let result_digest = result.digest();
    let transition = Transition { program: program_ref(), input: input_digest, result: result_digest };
    let committed = fixture.append(
        2,
        &AppendRecords::new(vec![result], vec![DriverRecord::Transition { cause: 1, record: transition.clone() }], 1),
    );
    assert_eq!(committed, AppendRecordsResult::Committed { head: 2, artifacts: vec![result_digest] });
    assert_eq!(fixture.head(), Seq(2));
    assert!(fixture.stored(&result_digest));

    let entries = fixture.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].seq, Seq(2));
    assert_eq!(entries[1].cause, Some(Seq(1)));
    assert_eq!(entries[1].decode::<Transition>().expect("decode transition"), transition);

    let dangling_result_digest = EncodedArtifact::opaque_bytes(b"never-staged").digest();
    let dangling = Transition { program: program_ref(), input: input_digest, result: dangling_result_digest };
    let sibling = EncodedArtifact::opaque_bytes(b"sibling-artifact");
    let sibling_digest = sibling.digest();
    let refused = fixture.append(
        3,
        &AppendRecords::new(vec![sibling], vec![DriverRecord::Transition { cause: 1, record: dangling }], 2),
    );
    assert!(
        matches!(refused, AppendRecordsResult::Err { .. }),
        "missing transition result must be refused: {refused:?}"
    );
    assert_eq!(fixture.head(), Seq(2));
    assert!(!fixture.stored(&sibling_digest));
}

#[test]
fn records_take_consecutive_seqs_in_request_order() {
    // Catches reordered records, a cause dropped or attached to the wrong
    // entry, a wrong reported head, or a head move not decoded as
    // RecordedHeadMove.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));

    let seed_input = EncodedArtifact::opaque_bytes(b"seed-input");
    let seed_digest = seed_input.digest();
    let seed = Requested { program: program_ref(), input: seed_digest, source: native_source() };
    assert_eq!(
        fixture.append(
            1,
            &AppendRecords::new(vec![seed_input], vec![DriverRecord::Requested { cause: None, record: seed }], 0)
        ),
        AppendRecordsResult::Committed { head: 1, artifacts: vec![seed_digest] }
    );

    let requested = Requested { program: program_ref(), input: Digest::from_bytes([5; 32]), source: native_source() };
    let reaction_failed =
        ReactionFailed { bundle: Digest::from_bytes([6; 32]), reactor: None, reason: Detail::new("reaction failed") };
    let activation_head: Head<OpaqueBytes> = Head::new("driver-activation");
    let activated = Activated::new(activation_head, Digest::from_bytes([7; 32]), Seq(1)).expect("activated");
    let move_head: Head<OpaqueBytes> = Head::new("driver-move");
    let head_target = EncodedArtifact::opaque_bytes(b"head-target");
    let head_target_digest = head_target.digest();
    let head_moved = RecordedHeadMove::new(RecordedHead::from(&move_head), head_target_digest);

    let records = vec![
        DriverRecord::Requested { cause: None, record: requested.clone() },
        DriverRecord::ReactionFailed { cause: 1, record: reaction_failed.clone() },
        DriverRecord::Activated { cause: 1, record: activated.clone() },
        DriverRecord::HeadMoved { cause: 1, record: head_moved.clone() },
    ];

    let committed = fixture.append(2, &AppendRecords::new(vec![head_target], records, 1));
    assert_eq!(committed, AppendRecordsResult::Committed { head: 5, artifacts: vec![head_target_digest] });

    let entries = fixture.entries();
    assert_eq!(entries.len(), 5);
    let tail = &entries[1..];

    assert_eq!(tail[0].seq, Seq(2));
    assert_eq!(tail[0].cause, None);
    assert_eq!(tail[0].decode::<Requested>().expect("decode requested"), requested);

    assert_eq!(tail[1].seq, Seq(3));
    assert_eq!(tail[1].cause, Some(Seq(1)));
    assert_eq!(tail[1].decode::<ReactionFailed>().expect("decode reaction failed"), reaction_failed);

    assert_eq!(tail[2].seq, Seq(4));
    assert_eq!(tail[2].cause, Some(Seq(1)));
    assert_eq!(tail[2].decode::<Activated>().expect("decode activated"), activated);

    assert_eq!(tail[3].seq, Seq(5));
    assert_eq!(tail[3].cause, Some(Seq(1)));
    assert_eq!(tail[3].decode::<RecordedHeadMove>().expect("decode head moved"), head_moved);
}
