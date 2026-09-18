//! Fold semantics of [`Activations`]: the latest activation per reactor head and any interval it owes.

use std::error::Error;

use aether_bloomery_kinds::{Activated, ActivationRejected, Detail, Digest, Entry, Head, OpaqueBytes, Seq};
use aether_bloomery_view::{ActivationFoldError, Activations, HeadActivation, SequenceError};
use aether_data::{Storage, StorageData};

fn entry_for<K: Storage + Clone>(seq: u64, cause: Option<u64>, event: &K) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: K::ID,
        cause: cause.map(Seq),
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone()))?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.activations.note")]
struct Note {
    n: u64,
}

fn digest(byte: u8) -> Digest {
    Digest::from_bytes([byte; 32])
}

fn filler(seq: u64) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, None, &Note { n: seq })
}

#[test]
fn a_rejection_owes_its_interval_to_the_next_activation() -> Result<(), Box<dyn Error>> {
    // Bug: a later rejection moving the owed start forward, an activation that skips the owed interval, and an
    // owed interval that never clears.
    let head = Head::<OpaqueBytes>::new("main");
    let mut activations = Activations::new();

    activations.apply(&entry_for(1, None, &Activated::new(head.clone(), digest(1), Seq(1))?)?)?;
    for seq in 2..=5 {
        activations.apply(&filler(seq)?)?;
    }
    activations.apply(&entry_for(
        6,
        Some(5),
        &ActivationRejected { head: head.clone(), bundle: digest(2), reason: Detail::new("rejected") },
    )?)?;
    assert!(matches!(activations.get(&head), Some(HeadActivation::Owed { from }) if *from == Seq(6)));

    activations.apply(&filler(7)?)?;
    activations.apply(&entry_for(
        8,
        Some(7),
        &ActivationRejected { head: head.clone(), bundle: digest(3), reason: Detail::new("rejected again") },
    )?)?;
    assert!(matches!(activations.get(&head), Some(HeadActivation::Owed { from }) if *from == Seq(6)));

    let skipped = activations
        .apply(&entry_for(9, None, &Activated::new(head.clone(), digest(4), Seq(8))?)?)
        .expect_err("skipping the owed interval must refuse");
    match skipped {
        ActivationFoldError::OwedMismatch { seq, head: mismatched_head, owed_from, live_from } => {
            assert_eq!(seq, Seq(9));
            assert_eq!(mismatched_head, head);
            assert_eq!(owed_from, Seq(6));
            assert_eq!(live_from, Seq(8));
        }
        other => panic!("expected OwedMismatch, got {other:?}"),
    }
    assert_eq!(activations.cursor(), Seq(8));
    assert!(matches!(activations.get(&head), Some(HeadActivation::Owed { from }) if *from == Seq(6)));

    activations.apply(&entry_for(9, None, &Activated::new(head.clone(), digest(5), Seq(6))?)?)?;
    assert!(matches!(activations.get(&head), Some(HeadActivation::Live(activated)) if activated.live_from() == Seq(6)));
    Ok(())
}

#[test]
fn a_gap_is_refused() -> Result<(), Box<dyn Error>> {
    // Bug: a fold that skips the shared next-sequence check.
    let head = Head::<OpaqueBytes>::new("main");
    let mut activations = Activations::new();
    activations.apply(&entry_for(1, None, &Activated::new(head.clone(), digest(1), Seq(1))?)?)?;

    let gap = activations.apply(&filler(3)?).expect_err("gap must refuse");
    assert!(
        matches!(gap, ActivationFoldError::Sequence(SequenceError::Gap { expected, actual }) if expected == Seq(2) && actual == Seq(3))
    );

    assert_eq!(activations.cursor(), Seq(1));
    assert!(matches!(activations.get(&head), Some(HeadActivation::Live(activated)) if activated.live_from() == Seq(1)));
    Ok(())
}
