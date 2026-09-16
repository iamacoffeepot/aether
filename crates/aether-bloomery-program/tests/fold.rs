//! Last write wins, including across a page boundary of 256.

use std::error::Error;

use aether_bloomery_journal::{Batch, Clock, Journal, Seq};
use aether_bloomery_kinds::{Mode, OpaqueBytes, ProgramName, ProgramNameMoved};
use aether_bloomery_program::named;
use aether_data::Kind;

const PAGE: usize = 256;

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

fn program(name: &str, intent: &str) -> Result<aether_bloomery_kinds::Program, Box<dyn Error>> {
    Ok(aether_bloomery_kinds::Program {
        name: ProgramName::new(name)?,
        input: OpaqueBytes::ID,
        result: OpaqueBytes::ID,
        mode: Mode::Pure,
        intent: intent.into(),
    })
}

#[test]
fn named_keeps_the_last_program_per_name_across_pages() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(1)))?;
    let a = program("trim", "first")?;
    let b = program("trim", "second")?;
    let c = program("hash", "third")?;
    let mut batch = Batch::new();
    let a_ref = batch.stage_encoded(&a)?;
    let b_ref = batch.stage_encoded(&b)?;
    let c_ref = batch.stage_encoded(&c)?;
    let filler = batch.stage_encoded(&program("fill", "page filler")?)?;
    for index in 0..PAGE {
        let name = ProgramName::new(format!("f{index:03}"))?;
        batch.push_event(&ProgramNameMoved { name, program: filler }, None)?;
    }
    batch.push_event(&ProgramNameMoved { name: ProgramName::new("trim")?, program: a_ref }, None)?;
    batch.push_event(&ProgramNameMoved { name: ProgramName::new("trim")?, program: b_ref }, None)?;
    batch.push_event(&ProgramNameMoved { name: ProgramName::new("hash")?, program: c_ref }, None)?;
    journal.append(Seq(0), &batch)?;

    let names = named(&journal)?;
    assert_eq!(names.get(&ProgramName::new("trim")?), Some(&b_ref));
    assert_eq!(names.get(&ProgramName::new("hash")?), Some(&c_ref));
    Ok(())
}
