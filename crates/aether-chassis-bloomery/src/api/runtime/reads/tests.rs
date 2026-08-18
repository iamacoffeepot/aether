//! Paging, ranging, and kind-resolution tests for the REST read surface.

use aether_bloomery::{
    BloomId, DECISIONS_SCHEMA, Decisions, Digest, Event, Fact, IdempotencyKey, JournalRecord, Outcome, StudyCost,
    StudyRecord,
};
use aether_data::wire::to_vec;

use super::artifacts::{ArtifactRange, range_bytes, resolve_kind};
use super::journal::page_journal;
use super::query::{
    ARTIFACT_DEFAULT_LIMIT, ARTIFACT_MAX_LIMIT, ArtifactQuery, JOURNAL_DEFAULT_LIMIT, JOURNAL_MAX_LIMIT, JournalQuery,
};

fn observe(sequence: u64, head: u8) -> JournalRecord {
    record(
        sequence,
        &Event {
            idempotency_key: IdempotencyKey(format!("obs-{sequence}")),
            fact: Fact::ObserveMainline { head: Digest::from_bytes([head; 32]) },
        },
    )
}

fn land(sequence: u64, bloom: Digest) -> JournalRecord {
    record(
        sequence,
        &Event {
            idempotency_key: IdempotencyKey(format!("land-{sequence}")),
            fact: Fact::Land { bloom: BloomId(bloom), new_head: Digest::from_bytes([9; 32]) },
        },
    )
}

fn record(sequence: u64, event: &Event) -> JournalRecord {
    let decisions = Decisions { outcome: Outcome::Sealed(BloomId(Digest::from_bytes([1; 32]))), effects: Vec::new() };
    JournalRecord {
        sequence,
        idempotency_key: event.idempotency_key.0.clone(),
        event: to_vec(event).expect("event encodes"),
        decisions: to_vec(&decisions).expect("decisions encode"),
        decider: "test".to_owned(),
        decisions_schema: Some(DECISIONS_SCHEMA.to_owned()),
        recorded_unix_millis: None,
    }
}

fn bare() -> JournalQuery {
    JournalQuery::parse("").expect("empty query is valid")
}

#[test]
fn a_bare_journal_query_is_the_newest_page() {
    // Tripwire: a bare GET /journal used to return the entire journal oldest
    // first. The console cannot absorb that; the default is the newest 100
    // with truncation flags.
    let records: Vec<JournalRecord> = (1_u8..=3).map(|n| observe(u64::from(n), n)).collect();
    let view = page_journal(&records, &bare()).expect("fixture records decode");
    assert_eq!(view.records.iter().map(|entry| entry.sequence).collect::<Vec<_>>(), vec![3, 2, 1]);
    assert_eq!(view.total_matched, 3);
    assert_eq!(view.shown, 3);
    assert!(!view.truncated);
    assert_eq!(view.next_from_sequence, None);
    assert_eq!(bare().limit, JOURNAL_DEFAULT_LIMIT);
}

#[test]
fn following_next_from_sequence_yields_every_record_once() {
    // Acceptance: paging to exhaustion visits each sequence exactly once.
    let records: Vec<JournalRecord> = (1_u8..=5).map(|n| observe(u64::from(n), n)).collect();
    let first = JournalQuery { limit: 2, ..bare() };
    let page_a = page_journal(&records, &first).expect("fixture records decode");
    assert_eq!(page_a.records.iter().map(|entry| entry.sequence).collect::<Vec<_>>(), vec![5, 4]);
    assert!(page_a.truncated);
    assert_eq!(page_a.next_from_sequence, Some(4));

    let second = JournalQuery { from_sequence: page_a.next_from_sequence, limit: 2, ..bare() };
    let page_b = page_journal(&records, &second).expect("fixture records decode");
    assert_eq!(page_b.records.iter().map(|entry| entry.sequence).collect::<Vec<_>>(), vec![3, 2]);
    assert_eq!(page_b.next_from_sequence, Some(2));

    let third = JournalQuery { from_sequence: page_b.next_from_sequence, limit: 2, ..bare() };
    let page_c = page_journal(&records, &third).expect("fixture records decode");
    assert_eq!(page_c.records.iter().map(|entry| entry.sequence).collect::<Vec<_>>(), vec![1]);
    assert!(!page_c.truncated);
    assert_eq!(page_c.next_from_sequence, None);

    let mut seen = Vec::new();
    seen.extend(page_a.records.iter().map(|entry| entry.sequence));
    seen.extend(page_b.records.iter().map(|entry| entry.sequence));
    seen.extend(page_c.records.iter().map(|entry| entry.sequence));
    seen.sort_unstable();
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_limit_above_the_journal_clamp_is_applied_and_named() {
    let query = JournalQuery::parse(&format!("limit={}", JOURNAL_MAX_LIMIT + 50)).expect("numeric limit parses");
    assert_eq!(query.limit, JOURNAL_MAX_LIMIT);
    assert_eq!(
        query.notice.as_deref(),
        Some(format!("limit clamped from {} to {JOURNAL_MAX_LIMIT}", JOURNAL_MAX_LIMIT + 50).as_str())
    );
}

#[test]
fn the_bloom_filter_keeps_only_events_that_name_it() {
    let wanted = Digest::from_bytes([7; 32]);
    let other = Digest::from_bytes([8; 32]);
    let records = vec![observe(1, 1), land(2, wanted), land(3, other), land(4, wanted)];
    let query = JournalQuery { bloom: Some(wanted), ..bare() };
    let view = page_journal(&records, &query).expect("fixture records decode");
    assert_eq!(view.records.iter().map(|entry| entry.sequence).collect::<Vec<_>>(), vec![4, 2]);
    assert_eq!(view.total_matched, 2);
}

#[test]
fn artifact_range_honors_bounds_and_rejects_past_the_end() {
    let bytes = b"abcdefghij";
    let query = ArtifactQuery { offset: 2, limit: 3, notice: None };
    match range_bytes(bytes, &query) {
        ArtifactRange::Ok { bytes, offset, total, truncated, .. } => {
            assert_eq!(bytes, b"cde");
            assert_eq!(offset, 2);
            assert_eq!(total, 10);
            assert!(truncated);
        }
        ArtifactRange::Unsatisfiable { .. } => panic!("an in-range offset must succeed"),
    }

    match range_bytes(bytes, &ArtifactQuery { offset: 10, limit: 1, notice: None }) {
        ArtifactRange::Unsatisfiable { total } => assert_eq!(total, 10),
        ArtifactRange::Ok { .. } => panic!("offset at the end is 416"),
    }
}

#[test]
fn an_artifact_limit_above_the_clamp_is_applied_and_named() {
    let query = ArtifactQuery::parse(&format!("limit={}", ARTIFACT_MAX_LIMIT + 1)).expect("numeric limit parses");
    assert_eq!(query.limit, ARTIFACT_MAX_LIMIT);
    assert!(query.notice.as_deref().is_some_and(|notice| notice.contains("clamped")));
    assert_eq!(ArtifactQuery::parse("").expect("empty query is valid").limit, ARTIFACT_DEFAULT_LIMIT);
}

#[test]
fn decoded_resolves_a_known_kind_and_reports_null_for_unknown_bytes() {
    let record = StudyRecord {
        bloom: BloomId(Digest::from_bytes([1; 32])),
        subject: Digest::from_bytes([2; 32]),
        cost: StudyCost::default(),
    };
    let bytes = to_vec(&record).expect("study record encodes");
    let (kind, value) = resolve_kind(&bytes).expect("a study record is a known kind");
    assert_eq!(kind, "aether.bloomery.study_record");
    let subject: Vec<u8> = serde_json::from_value(value["subject"].clone()).expect("subject is a byte array");
    assert_eq!(subject, vec![2; 32]);

    assert_eq!(resolve_kind(b"not-an-artifact"), None);
}
