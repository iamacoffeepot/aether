//! Stream binding, range checks, poison, and pending peer evaluation.

use std::error::Error;
use std::fmt;

use aether_bloomery_kinds::{Digest, Entry, Head, HeadMoved, Ref, Seq, Tree};
use aether_bloomery_reactor::{
    Cluster, Event, EventBatch, Guard, GuardArg, JournalEntry, PeerEvaluated, PrepareError, PreparedPrefix,
};
use aether_bloomery_view::View;
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_data::{Kind, KindId, MailboxId, Storage, StorageData};

const PEER_A: MailboxId = MailboxId(11);
const PEER_B: MailboxId = MailboxId(12);
const PEER_C: MailboxId = MailboxId(13);

#[derive(Debug)]
struct Boom;

impl fmt::Display for Boom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("boom")
    }
}

impl Error for Boom {}

struct Flaky {
    cursor: Seq,
}

impl View for Flaky {
    type Error = Boom;

    fn empty() -> Self {
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        if entries.iter().any(|entry| entry.seq.0 >= 2) {
            return Err(Boom);
        }
        if let Some(last) = entries.last() {
            self.cursor = last.seq;
        }
        Ok(())
    }
}

struct NeedsFlaky;

impl Guard<HeadMoved<Tree>> for NeedsFlaky {
    type Views = Flaky;

    fn resolve(_trigger: &HeadMoved<Tree>, _view: &Flaky) -> Option<Self> {
        Some(Self)
    }
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn moved(seq: u64, to: Ref<Tree>) -> Result<Entry, Box<dyn Error>> {
    let event = Head::<Tree>::new("source").move_to(to);
    Ok(Entry {
        seq: Seq(seq),
        kind: HeadMoved::<Tree>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: HeadMoved::<Tree>::encode_storage(&StorageData::from_value(event))?,
    })
}

#[test]
fn reactor_mail_envelopes_preserve_full_width_kind_ids() -> Result<(), Box<dyn Error>> {
    let entry = Entry {
        seq: Seq(7),
        kind: KindId(0xfedc_ba98_7654_3210),
        cause: Some(Seq(3)),
        recorded_at_millis: 42,
        bytes: vec![1, 2, 3],
    };
    let portable = JournalEntry::from_entry(&entry);
    assert_eq!(portable.to_entry(), entry);

    let event = Event { stream: "alpha".into(), entry: portable };
    let event_bytes = encode_to_vec(&event)?;
    let decoded_event = decode_from_slice::<Event>(&event_bytes)?;
    assert_eq!(decoded_event.entry.to_entry(), entry);

    let prepared = PreparedPrefix::from_parts("alpha", &entry, Vec::new());
    let prepared_bytes = encode_to_vec(&prepared)?;
    let decoded_prepared = decode_from_slice::<PreparedPrefix>(&prepared_bytes)?;
    assert_eq!(decoded_prepared.to_entry(), entry);
    Ok(())
}

#[test]
fn empty_stream_is_refused_before_binding() {
    let cluster = Cluster::new();
    let error = cluster.check_admit("").expect_err("empty");
    assert!(matches!(error, PrepareError::InvalidStream), "{error}");
    assert!(cluster.stream().is_none());
}

#[test]
fn bound_stream_rejects_a_mismatch() {
    let mut cluster = Cluster::new();
    cluster.check_admit("alpha").expect("first");
    cluster.bind_stream("alpha");
    let error = cluster.check_admit("beta").expect_err("mismatch");
    assert!(
        matches!(&error, PrepareError::StreamMismatch { bound, actual } if bound == "alpha" && actual == "beta"),
        "{error}"
    );
}

#[test]
fn poisoned_cluster_refuses_further_admission() {
    let mut cluster = Cluster::new();
    cluster.bind_stream("alpha");
    cluster.mark_poisoned();
    let error = cluster.check_admit("alpha").expect_err("poisoned");
    assert!(
        matches!(error, PrepareError::PoisonedCluster { last_trusted_cursor } if last_trusted_cursor == Seq(0)),
        "{error}"
    );
    assert!(cluster.status().poisoned);
}

#[test]
fn partial_fold_failure_keeps_last_trusted_cursor() -> Result<(), Box<dyn Error>> {
    let mut cluster = Cluster::new();
    cluster.bind_stream("alpha");
    cluster.owner_mut().push(&[moved(1, digest_ref::<Tree>(1))?])?;
    cluster.owner_mut().prepare::<HeadMoved<Tree>, GuardArg<NeedsFlaky>>()?.expect("first fold");
    cluster.trust_cursor();
    assert_eq!(cluster.owner().cursor(), Seq(1));

    cluster.owner_mut().push(&[moved(2, digest_ref::<Tree>(2))?])?;
    let failed = cluster.owner_mut().prepare::<HeadMoved<Tree>, GuardArg<NeedsFlaky>>().err().expect("second fold");
    assert!(
        matches!(&failed, PrepareError::Advance { last_trusted_cursor, .. } if *last_trusted_cursor == Seq(1)),
        "{failed}"
    );
    assert_eq!(cluster.owner().cursor(), Seq(2));
    assert!(cluster.owner().is_poisoned());

    cluster.mark_poisoned();
    let refused = cluster.check_admit("alpha").expect_err("poisoned");
    assert!(
        matches!(&refused, PrepareError::PoisonedCluster { last_trusted_cursor } if *last_trusted_cursor == Seq(1)),
        "{refused}"
    );
    assert_eq!(cluster.owner().cursor(), Seq(2));
    let still = cluster.check_admit("alpha").expect_err("still poisoned");
    assert!(
        matches!(&still, PrepareError::PoisonedCluster { last_trusted_cursor } if *last_trusted_cursor == Seq(1)),
        "{still}"
    );
    Ok(())
}

#[test]
fn event_batch_rejects_empty_and_inconsistent_ranges() {
    let empty = EventBatch { stream: String::from("alpha"), from: 1, through: 1, entries: Vec::new() };
    assert!(matches!(empty.validate(), Err(PrepareError::InvalidRange { .. })));

    let entries = vec![
        JournalEntry { seq: 1, kind: Tree::ID, cause: None, recorded_at_millis: 0, bytes: Vec::new() },
        JournalEntry { seq: 3, kind: Tree::ID, cause: None, recorded_at_millis: 0, bytes: Vec::new() },
    ];
    let gapped = EventBatch { stream: String::from("alpha"), from: 1, through: 3, entries };
    assert!(matches!(gapped.validate(), Err(PrepareError::InvalidRange { .. })));
}

#[test]
fn unique_peer_outcomes_complete_once() {
    let mut cluster = Cluster::new();
    assert!(cluster.start_live("alpha", 2, &[PEER_A, PEER_B]).is_none());
    assert!(cluster.note_peer(PEER_A, &PeerEvaluated::ok("alpha", 2)).is_none());
    let done = cluster.note_peer(PEER_B, &PeerEvaluated::ok("alpha", 2)).expect("second");
    assert!(done.is_ok());
    assert!(cluster.note_peer(PEER_A, &PeerEvaluated::ok("alpha", 2)).is_none());
}

#[test]
fn duplicate_one_missing_other_is_not_success() {
    let mut cluster = Cluster::new();
    assert!(cluster.start_live("alpha", 4, &[PEER_A, PEER_B]).is_none());
    assert!(cluster.note_peer(PEER_A, &PeerEvaluated::ok("alpha", 4)).is_none());
    assert!(cluster.note_peer(PEER_A, &PeerEvaluated::ok("alpha", 4)).is_none());
    assert!(cluster.note_peer(PEER_A, &PeerEvaluated::from_error("alpha", 4, &PrepareError::Empty)).is_none());
}

#[test]
fn wrong_peer_or_stream_is_not_accepted() {
    let mut cluster = Cluster::new();
    assert!(cluster.start_live("alpha", 5, &[PEER_A, PEER_B]).is_none());
    assert!(cluster.note_peer(PEER_C, &PeerEvaluated::ok("alpha", 5)).is_none());
    assert!(cluster.note_peer(PEER_A, &PeerEvaluated::ok("beta", 5)).is_none());
    assert!(cluster.note_peer(PEER_A, &PeerEvaluated::ok("alpha", 9)).is_none());
    assert!(cluster.note_peer(PEER_B, &PeerEvaluated::ok("alpha", 5)).is_none());
    let done = cluster.note_peer(PEER_A, &PeerEvaluated::ok("alpha", 5)).expect("original peer still expected");
    assert!(done.is_ok());
}

#[test]
fn pending_peer_failure_is_not_success() {
    let mut cluster = Cluster::new();
    assert!(cluster.start_live("alpha", 4, &[PEER_A, PEER_B]).is_none());
    let fail = PeerEvaluated::from_error("alpha", 4, &PrepareError::Empty);
    assert!(cluster.note_peer(PEER_A, &fail).is_none());
    let done = cluster.note_peer(PEER_B, &PeerEvaluated::ok("alpha", 4)).expect("second");
    assert!(!done.is_ok(), "{done:?}");
}

#[test]
fn zero_peers_cannot_succeed() {
    let mut cluster = Cluster::new();
    let done = cluster.start_live("alpha", 1, &[]).expect("missing peers");
    assert!(!done.is_ok(), "{done:?}");
}
