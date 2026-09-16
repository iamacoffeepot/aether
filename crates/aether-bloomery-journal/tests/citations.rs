//! Citation verification: dangling, prefix mismatch, order, and nested walks.

mod common;

use std::error::Error;

use aether_bloomery_journal::{AppendError, Batch, Digest, Journal, OpaqueBytes, Ref, Seq, Utf8Text};
use aether_data::Kind;
use common::FixedClock;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.referenced")]
struct Referenced {
    digest: Ref<OpaqueBytes>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.cite_text")]
struct CiteText {
    text: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.nested")]
enum Nested {
    Many(Vec<Ref<OpaqueBytes>>),
    Named { item: Ref<Utf8Text> },
}

#[test]
fn a_batch_whose_event_cites_an_absent_digest_is_refused_whole() -> Result<(), Box<dyn Error>> {
    // Catches insert-then-verify with no rollback, which leaves orphan rows a
    // later retry silently reuses.
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"should-not-land");
    let missing = Ref::<OpaqueBytes>::from_digest(Digest::from_bytes([7; 32]));
    batch.push_event(&Referenced { digest: missing }, None)?;

    let error = journal.append(Seq(0), &batch).expect_err("dangling ref must fail");
    match error {
        AppendError::DanglingRef { digest, expected } => {
            assert_eq!(digest, missing.digest());
            assert_eq!(expected, OpaqueBytes::ID);
        }
        other => panic!("expected DanglingRef, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    assert_eq!(journal.get_bytes(&staged.digest())?, None);
    Ok(())
}

#[test]
fn a_citation_whose_target_blob_carries_another_prefix_is_refused() -> Result<(), Box<dyn Error>> {
    // Catches a verifier that checks existence only.
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
    let mut batch = Batch::new();
    let text = batch.stage_text("hello");
    let as_bytes = Ref::<OpaqueBytes>::from_digest(text.digest());
    batch.push_event(&Referenced { digest: as_bytes }, None)?;

    let error = journal.append(Seq(0), &batch).expect_err("prefix mismatch must fail");
    match error {
        AppendError::PrefixMismatch { digest, expected, actual } => {
            assert_eq!(digest, text.digest());
            assert_eq!(expected, OpaqueBytes::ID);
            assert_eq!(actual, Utf8Text::ID);
        }
        other => panic!("expected PrefixMismatch, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    assert_eq!(journal.get_bytes(&text.digest())?, None);
    Ok(())
}

#[test]
fn an_artifact_staged_earlier_may_cite_an_artifact_staged_later() -> Result<(), Box<dyn Error>> {
    // Catches a verifier that runs per insert instead of after all inserts,
    // which would fail a correct batch on entry order alone.
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
    let mut batch = Batch::new();
    let later = Ref::of_text("hello");
    batch.stage_encoded(&CiteText { text: later })?;
    batch.stage_text("hello");
    journal.append(Seq(0), &batch)?;
    assert_eq!(
        journal.get::<CiteText>(&Ref::of_encoded(&CiteText { text: later })?.digest())?,
        Some(CiteText { text: later })
    );
    Ok(())
}

#[test]
fn a_ref_nested_inside_a_vec_and_inside_an_enum_variant_is_found() -> Result<(), Box<dyn Error>> {
    // A walker that only visits top-level struct fields returns no citation
    // and the batch wrongly commits. A Ref inside a container never passes
    // through a RecordWriter.
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
    let missing_bytes = Ref::<OpaqueBytes>::from_digest(Digest::from_bytes([3; 32]));
    let mut batch = Batch::new();
    batch.push_event(&Nested::Many(vec![missing_bytes]), None)?;
    let error = journal.append(Seq(0), &batch).expect_err("nested vec ref must dangle");
    match error {
        AppendError::DanglingRef { digest, expected } => {
            assert_eq!(digest, missing_bytes.digest());
            assert_eq!(expected, OpaqueBytes::ID);
        }
        other => panic!("expected DanglingRef, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));

    let missing_text = Ref::<Utf8Text>::from_digest(Digest::from_bytes([4; 32]));
    let mut batch = Batch::new();
    batch.push_event(&Nested::Named { item: missing_text }, None)?;
    let error = journal.append(Seq(0), &batch).expect_err("nested enum ref must dangle");
    match error {
        AppendError::DanglingRef { digest, expected } => {
            assert_eq!(digest, missing_text.digest());
            assert_eq!(expected, Utf8Text::ID);
        }
        other => panic!("expected DanglingRef, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    Ok(())
}
