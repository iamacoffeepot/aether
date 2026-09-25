//! Read limit and past-head behavior.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Batch, Draft, Journal, Seq};

fn batch_from_drafts(drafts: impl IntoIterator<Item = Draft>) -> Batch {
    let mut batch = Batch::new();
    for draft in drafts {
        batch.push_draft(draft);
    }
    batch
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.note")]
struct Note {
    text: String,
}

impl Note {
    fn draft(text: &str) -> Draft {
        Draft::of(&Self { text: text.to_owned() }, None).expect("encode note")
    }
}

#[test]
fn read_with_a_short_limit_returns_that_many_ascending_and_past_the_head_is_empty() -> Result<(), Box<dyn Error>> {
    let (_root, mut journal) = common::temp_journal(0)?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("a"), Note::draft("b"), Note::draft("c")]))?;

    let page = journal.read(Seq(0), 2)?;
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].seq, Seq(1));
    assert_eq!(page[1].seq, Seq(2));
    assert_eq!(Journal::decode::<Note>(&page[0])?.text, "a");
    assert_eq!(Journal::decode::<Note>(&page[1])?.text, "b");

    let rest = journal.read(Seq(2), 10)?;
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].seq, Seq(3));

    assert!(journal.read(Seq(3), 10)?.is_empty());
    assert!(journal.read(Seq(99), 10)?.is_empty());
    Ok(())
}
