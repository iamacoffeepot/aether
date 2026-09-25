//! A batch whose event cites an artifact staged in the same batch.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Batch, Journal, OpaqueBytes, Ref, Seq};
use aether_data::Kind;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.referenced")]
struct Referenced {
    digest: Ref<OpaqueBytes>,
}

#[test]
fn a_batch_event_cites_an_artifact_staged_in_the_same_batch_and_the_citation_resolves() -> Result<(), Box<dyn Error>> {
    // Catches a batch that writes events and blobs in separate transactions
    // or drops the staged blob.
    let (_root, mut journal) = common::temp_journal(0)?;
    let payload = b"transcript";
    let mut batch = Batch::new();
    let digest = batch.stage_bytes(payload);
    batch.push_event(&Referenced { digest }, None)?;
    journal.append(Seq(0), &batch)?;

    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("one entry");
    let decoded = Journal::decode::<Referenced>(&entry)?;
    assert_eq!(decoded.digest, digest);
    assert_eq!(journal.get_bytes(&decoded.digest.digest())?, Some((OpaqueBytes::ID, payload.to_vec())));
    Ok(())
}
