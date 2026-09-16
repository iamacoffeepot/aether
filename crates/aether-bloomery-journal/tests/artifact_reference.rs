//! An event carrying a Digest round-trips and resolves through [`Journal::get_artifact`].

mod common;

use std::error::Error;

use aether_bloomery_journal::{Digest, Draft, Journal, Seq};
use common::FixedClock;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.referenced")]
struct Referenced {
    digest: Digest,
}

#[test]
fn an_event_with_a_digest_field_round_trips_and_the_digest_resolves() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
    let payload = b"transcript";
    let digest = journal.put_artifact(payload)?;
    journal.append(Seq(0), &[Draft::of(&Referenced { digest }, None)?])?;

    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("one entry");
    let decoded = Journal::decode::<Referenced>(&entry)?;
    assert_eq!(decoded.digest, digest);
    assert_eq!(journal.get_artifact(&decoded.digest)?.as_deref(), Some(payload.as_slice()));
    Ok(())
}
