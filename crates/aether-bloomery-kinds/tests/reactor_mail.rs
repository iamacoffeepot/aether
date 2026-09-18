//! Reactor protocol mail: `WarmEntries` validation and pinned kind ids.

use aether_bloomery_kinds::{
    Evaluated, Event, JournalEntry, Status, StatusQuery, Warm, WarmEntries, WarmEntriesError, Warmed,
};
use aether_data::{Kind, KindId};

fn entry(seq: u64) -> JournalEntry {
    JournalEntry { seq, kind: KindId(1), cause: None, recorded_at_millis: 0, bytes: Vec::new() }
}

#[aether_data::kind(name = "test.bloomery.reactor.unchecked_warm", eq)]
struct UncheckedWarm {
    entries: Vec<JournalEntry>,
}

fn refuse_batch(entries: Vec<JournalEntry>) {
    assert!(WarmEntries::new(entries.clone()).is_err(), "construct {entries:?}");
    let bytes = UncheckedWarm { entries }.encode_into_bytes();
    assert!(Warm::decode_from_bytes(&bytes).is_none(), "decode {bytes:?}");
}

#[test]
fn warm_entries_refuse_empty_and_non_dense_batches_on_construct_and_decode() {
    refuse_batch(Vec::new());
    refuse_batch(vec![entry(0)]);
    refuse_batch(vec![entry(1), entry(3)]);
    refuse_batch(vec![entry(1), entry(1)]);

    let dense = vec![entry(1), entry(2), entry(3)];
    let accepted = WarmEntries::new(dense.clone()).expect("dense construct");
    assert_eq!(accepted.first(), 1);
    assert_eq!(accepted.last(), 3);
    let bytes = UncheckedWarm { entries: dense }.encode_into_bytes();
    let warm = Warm::decode_from_bytes(&bytes).expect("dense decode");
    assert_eq!(warm.entries().as_slice().len(), 3);
    assert_eq!(WarmEntries::new(Vec::new()), Err(WarmEntriesError::Empty));
    assert_eq!(WarmEntries::new(vec![entry(0)]), Err(WarmEntriesError::ZeroFirst));
    assert_eq!(WarmEntries::new(vec![entry(1), entry(3)]), Err(WarmEntriesError::NotDense));
}

#[test]
fn reactor_protocol_kind_ids_are_pinned() {
    // Tripwire: bundles built against a drifted schema can't load beside older ones
    // (ADR-0225 decision 5).
    assert_eq!(Warm::ID, TRIPWIRE_WARM);
    assert_eq!(Warmed::ID, TRIPWIRE_WARMED);
    assert_eq!(Event::ID, TRIPWIRE_EVENT);
    assert_eq!(Evaluated::ID, TRIPWIRE_EVALUATED);
    assert_eq!(StatusQuery::ID, TRIPWIRE_STATUS_QUERY);
    assert_eq!(Status::ID, TRIPWIRE_STATUS);
}

const TRIPWIRE_WARM: KindId = KindId(0x2ff5_d120_60f2_d51e);
const TRIPWIRE_WARMED: KindId = KindId(0x2e92_8abc_d04c_43a0);
const TRIPWIRE_EVENT: KindId = KindId(0x241b_bd4d_0535_3f94);
const TRIPWIRE_EVALUATED: KindId = KindId(0x2381_9f0f_18d4_93d3);
const TRIPWIRE_STATUS_QUERY: KindId = KindId(0x2e1c_78f0_8320_e7c6);
const TRIPWIRE_STATUS: KindId = KindId(0x2007_56da_bbf2_ec0f);
