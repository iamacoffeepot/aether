//! [`HeadHistory`] answers the [`Heads`] of every folded prefix.

use std::error::Error;

use aether_bloomery_kinds::{Digest, Entry, Head, Program, ProgramHeadMoved, ProgramName, Ref, Seq, Tree};
use aether_bloomery_view::{HeadHistory, Heads};
use aether_data::{Kind, Storage, StorageData};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.history_note")]
struct Note {
    n: u64,
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn entry_for<K: Storage + Clone>(seq: u64, event: &K) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: K::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone()))?,
    })
}

fn moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Head::<K>::new(name).move_to(to))
}

fn note(seq: u64, n: u64) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Note { n })
}

#[test]
fn heads_at_matches_a_fold_to_every_prefix() -> Result<(), Box<dyn Error>> {
    // Bugs: an off-by-one at the boundary (`..seq` instead of `..=seq`), a
    // head's latest move returned instead of its move as of the seq, or a head
    // that had not moved by the seq appearing in the result.
    let entries = vec![
        note(1, 1)?,
        moved::<Program>(2, "main", digest_ref(1))?,
        note(3, 3)?,
        moved::<Tree>(4, "src", digest_ref(2))?,
        moved::<Program>(5, "main", digest_ref(3))?,
        note(6, 6)?,
        entry_for(7, &ProgramHeadMoved { name: ProgramName::new("trim")?, program: digest_ref(4) })?,
        moved::<Tree>(8, "src", digest_ref(5))?,
        moved::<Program>(9, "main", digest_ref(6))?,
        note(10, 10)?,
        moved::<Program>(11, "main", digest_ref(1))?,
        note(12, 12)?,
    ];
    let mut history = HeadHistory::new();
    for entry in &entries {
        history.apply(entry)?;
    }

    let mut folded = Heads::new();
    assert_eq!(history.heads_at(Seq(0)), Some(folded.clone()));
    for entry in &entries {
        folded.apply(entry)?;
        assert_eq!(history.heads_at(entry.seq), Some(folded.clone()), "heads at {}", entry.seq);
    }
    assert_eq!(history.cursor(), Seq(12));
    assert_eq!(history.heads_at(Seq(13)), None);
    Ok(())
}
