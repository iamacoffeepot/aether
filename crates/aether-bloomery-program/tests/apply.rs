//! End-to-end apply of a Pure trim program.

mod common;

use std::collections::BTreeMap;
use std::error::Error;

use aether_bloomery_journal::{Batch, Journal, Seq};
use aether_bloomery_kinds::{ExecutorName, Name, Node, ProgramNamed, Transition};
use aether_bloomery_program::{Applied, Executors, apply, declaration, digest, named};
use aether_data::Kind;
use common::{FixedClock, Trim, TrimExecutor, TrimInput, TrimResult};

#[test]
fn apply_trims_files_records_a_transition_and_names_fold_after_program_named() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(1)))?;
    let mut batch = Batch::new();
    let file = batch.stage_bytes(b"hello  \n");
    let mut entries = BTreeMap::new();
    entries.insert(Name::new("a")?, Node::File(file));
    let tree = batch.stage_encoded(&aether_bloomery_kinds::Tree::new(entries)?)?;
    let input = batch.stage_encoded(&TrimInput { tree })?;
    batch.stage_encoded(&declaration::<Trim>())?;
    journal.append(Seq(0), &batch)?;

    let mut executors = Executors::new();
    executors.register::<Trim, _>(ExecutorName::new("in_process")?, TrimExecutor);
    let applied = apply(&mut journal, &executors, digest::<Trim>(), input.digest(), None)?;
    let Applied::Transition(seq) = applied else {
        panic!("expected Transition, got {applied:?}");
    };

    let entries = journal.read(Seq(0), 16)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seq, seq);
    assert_eq!(entries[0].kind, Transition::NAME);
    let transition = Journal::decode::<Transition>(&entries[0])?;
    let result = journal.get::<TrimResult>(&transition.result)?.expect("result stored");
    assert_eq!(result.changed, 1);
    let trimmed = journal.get::<aether_bloomery_kinds::Tree>(&result.tree.digest())?.expect("result tree");
    let Node::File(file) = trimmed.entries().values().next().expect("one file") else {
        panic!("expected a file");
    };
    let (_, payload) = journal.get_bytes(&file.digest())?.expect("file bytes");
    assert_eq!(payload, b"hello");

    assert!(named(&journal)?.is_empty());
    let mut named_batch = Batch::new();
    named_batch.push_event(
        &ProgramNamed { name: aether_bloomery_kinds::ProgramName::new("trim")?, program: digest::<Trim>() },
        None,
    )?;
    journal.append(journal.head()?, &named_batch)?;
    let names = named(&journal)?;
    assert_eq!(names.get(&aether_bloomery_kinds::ProgramName::new("trim")?), Some(&digest::<Trim>()));
    Ok(())
}
