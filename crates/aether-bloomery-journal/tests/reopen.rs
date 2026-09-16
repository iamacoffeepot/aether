//! File-backed reopen preserves the head.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Journal, Seq};
use common::{FixedClock, Note};

#[test]
fn a_file_backed_journal_closed_and_reopened_at_the_same_path_reports_the_same_head() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("journal.sqlite");

    {
        let mut journal = Journal::open_with_clock(&path, Box::new(FixedClock))?;
        journal.append(Seq(0), &[Note::draft("persist")])?;
        assert_eq!(journal.head()?, Seq(1));
    }

    let journal = Journal::open_with_clock(&path, Box::new(FixedClock))?;
    assert_eq!(journal.head()?, Seq(1));
    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("persisted entry");
    assert_eq!(Journal::decode::<Note>(&entry)?.text, "persist");
    Ok(())
}
