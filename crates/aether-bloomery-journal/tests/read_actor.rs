//! Two named owners answer isolated, correlated pages from their own files.

mod actor_support;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use aether_bloomery_journal::{Batch, Clock, Draft, Journal, JournalActor, Seq};
use aether_bloomery_kinds::{JournalEntry, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult};
use aether_data::Kind;
use aether_substrate::mail::registry::OwnedDispatch;
use aether_substrate::testing::{bare_substrate, boot_test_chassis_with};
use aether_substrate::{SpawnError, Subname};

use actor_support::{TestAnchor, caller, reply, request};

const STAMP_MILLIS: u64 = 1_700_000_000_000;

struct FixedClock;

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        STAMP_MILLIS
    }
}

#[derive(Clone, Debug, aether_data::Storage)]
#[kind(name = "test.bloomery.journal_actor.note")]
struct Note {
    text: String,
}

#[derive(Clone, Debug, aether_data::Storage)]
#[kind(name = "test.bloomery.journal_actor.marker")]
struct Marker {
    value: u64,
}

fn seed(path: &Path, notes: &[&str]) -> Vec<aether_bloomery_journal::Entry> {
    let mut journal = Journal::open_with_clock(path, Box::new(FixedClock)).expect("create seed journal");
    let mut batch = Batch::new();
    for (index, note) in notes.iter().enumerate() {
        batch.push_draft(
            Draft::of(&Note { text: (*note).to_owned() }, (index > 0).then_some(Seq(index as u64)))
                .expect("encode note"),
        );
    }
    batch.push_draft(Draft::of(&Marker { value: 47 }, Some(Seq(notes.len() as u64))).expect("encode marker"));
    journal.append(Seq(0), &batch).expect("seed entries");
    journal.read(Seq(0), 128).expect("read seed parity")
}

fn replies_by_correlation<K: Kind>(rx: &mpsc::Receiver<OwnedDispatch>, correlations: [u64; 2]) -> (K, K) {
    assert_ne!(correlations[0], correlations[1]);
    let mut replies = BTreeMap::new();
    for _ in 0..correlations.len() {
        let dispatch = rx.recv_timeout(Duration::from_secs(2)).expect("reply within two seconds");
        assert_eq!(dispatch.kind, K::ID);
        let correlation = dispatch.sender.correlation_id;
        assert!(correlations.contains(&correlation), "unexpected correlation {correlation}");
        let decoded = K::decode_from_bytes(dispatch.payload.bytes()).expect("decode reply");
        assert!(replies.insert(correlation, decoded).is_none(), "duplicate correlation {correlation}");
    }
    (
        replies.remove(&correlations[0]).expect("first correlated reply"),
        replies.remove(&correlations[1]).expect("second correlated reply"),
    )
}

#[test]
fn named_journals_return_isolated_pages_and_correlated_replies() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let alpha_path = temp.path().join("alpha.sqlite");
    let beta_path = temp.path().join("beta.sqlite");
    let alpha_expected = seed(&alpha_path, &["alpha one", "alpha two"]);
    let beta_expected = seed(&beta_path, &["beta one"]);

    let (registry, mailer) = bare_substrate();
    let (first_caller, first_rx) = caller(&registry, "test.journal_actor.caller_first");
    let (second_caller, second_rx) = caller(&registry, "test.journal_actor.caller_second");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let alpha =
        chassis.spawn_actor::<JournalActor>(Subname::Named("alpha"), alpha_path, ()).finish().expect("alpha birth");
    let beta = chassis.spawn_actor::<JournalActor>(Subname::Named("beta"), beta_path, ()).finish().expect("beta birth");
    assert_ne!(alpha, beta);

    // Four outstanding requests exercise both addresses and two independent reply targets.
    request(&registry, alpha, first_caller, 11, &ReadEvents { after: 0, limit: 2 });
    request(&registry, beta, second_caller, 22, &ReadHead);
    request(&registry, beta, first_caller, 33, &ReadEvents { after: 0, limit: 2 });
    request(&registry, alpha, second_caller, 44, &ReadHead);

    let (first_alpha, first_beta): (ReadEventsResult, ReadEventsResult) = replies_by_correlation(&first_rx, [11, 33]);
    let (second_beta, second_alpha): (ReadHeadResult, ReadHeadResult) = replies_by_correlation(&second_rx, [22, 44]);
    assert_eq!(second_beta, ReadHeadResult::Ok { head: beta_expected.len() as u64 });
    assert_eq!(second_alpha, ReadHeadResult::Ok { head: alpha_expected.len() as u64 });
    assert_eq!(
        first_alpha,
        ReadEventsResult::Ok {
            after: 0,
            head: alpha_expected.len() as u64,
            entries: alpha_expected[..2].iter().map(JournalEntry::from_entry).collect(),
        }
    );
    assert_eq!(
        first_beta,
        ReadEventsResult::Ok {
            after: 0,
            head: beta_expected.len() as u64,
            entries: beta_expected.iter().map(JournalEntry::from_entry).collect(),
        }
    );

    request(&registry, alpha, first_caller, 55, &ReadEvents { after: 2, limit: 1 });
    let tail: ReadEventsResult = reply(&first_rx, 55);
    let ReadEventsResult::Ok { after: 2, head: 3, entries } = tail else {
        panic!("expected alpha's final page, got {tail:?}");
    };
    assert_eq!(entries, [JournalEntry::from_entry(&alpha_expected[2])]);
    assert_eq!(entries[0].seq, 3);
    assert_eq!(entries[0].kind, Marker::ID);
    assert_eq!(entries[0].cause, Some(2));
    assert_eq!(entries[0].recorded_at_millis, STAMP_MILLIS);
    assert_eq!(entries[0].to_entry(), alpha_expected[2]);

    request(&registry, alpha, first_caller, 66, &ReadEvents { after: 3, limit: 1 });
    assert_eq!(reply::<ReadEventsResult>(&first_rx, 66), ReadEventsResult::Ok { after: 3, head: 3, entries: vec![] });
    request(&registry, beta, first_caller, 77, &ReadEvents { after: 99, limit: 1 });
    assert_eq!(reply::<ReadEventsResult>(&first_rx, 77), ReadEventsResult::Ok { after: 99, head: 2, entries: vec![] });

    for limit in [0, 129] {
        request(&registry, alpha, first_caller, 100 + u64::from(limit), &ReadEvents { after: 1, limit });
        assert!(matches!(
            reply::<ReadEventsResult>(&first_rx, 100 + u64::from(limit)),
            ReadEventsResult::Err { after: 1, message } if message.contains("limit")
        ));
    }
}

#[test]
fn invalid_path_fails_actor_birth() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let invalid_path = temp.path().join("missing-parent").join("journal.sqlite");
    let (registry, mailer) = bare_substrate();
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());

    let result = chassis.spawn_actor::<JournalActor>(Subname::Named("invalid"), invalid_path, ()).finish();
    assert!(matches!(result, Err(SpawnError::InitFailed(_))), "invalid path must fail birth: {result:?}");
}
