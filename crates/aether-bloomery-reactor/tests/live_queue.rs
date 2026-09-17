//! The portable live FIFO preserves exact envelopes through wrap and spill.

use aether_bloomery_reactor::{JournalEntry, LiveQueue};
use aether_data::KindId;

fn entry(seq: u64, len: usize) -> JournalEntry {
    let seed = u8::try_from(seq % 256).expect("sequence byte remainder fits");
    JournalEntry {
        seq,
        kind: KindId(seq + 100),
        cause: Some(seq.saturating_sub(1)),
        recorded_at_millis: seq * 17,
        bytes: (0..len)
            .map(|index| u8::try_from(index % 256).expect("index byte remainder fits").wrapping_add(seed))
            .collect(),
    }
}

#[test]
fn wrap_reclaims_storage_without_changing_fifo_or_metadata() {
    let mut queue = LiveQueue::new();
    let first = entry(1, 10_000);
    let second = entry(2, 4_000);
    let wrapped = entry(3, 3_000);
    queue.push(first.clone());
    queue.push(second.clone());
    assert_eq!(queue.pop(), Some(first));
    queue.push(wrapped.clone());
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.pop(), Some(second));
    assert_eq!(queue.pop(), Some(wrapped));
    assert!(queue.is_empty());

    let reused = entry(4, 16_384);
    queue.push(reused.clone());
    assert_eq!(queue.pop(), Some(reused));
}

#[test]
fn byte_pressure_and_oversize_payloads_spill_in_place() {
    let mut queue = LiveQueue::new();
    let entries = [entry(1, 12_000), entry(2, 8_000), entry(3, 20_000), entry(4, 0)];
    for item in &entries {
        queue.push(item.clone());
    }
    for item in entries {
        assert_eq!(queue.pop(), Some(item));
    }
    assert!(queue.pop().is_none());
}

#[test]
fn full_descriptor_ring_retains_every_later_entry() {
    let mut queue = LiveQueue::new();
    for seq in 1..=140 {
        queue.push(entry(seq, (seq % 11) as usize));
    }
    assert_eq!(queue.len(), 140);
    for seq in 1..=140 {
        assert_eq!(queue.pop(), Some(entry(seq, (seq % 11) as usize)));
    }
    assert!(queue.is_empty());
    queue.push(entry(141, 24));
    assert_eq!(queue.pop(), Some(entry(141, 24)));
}
