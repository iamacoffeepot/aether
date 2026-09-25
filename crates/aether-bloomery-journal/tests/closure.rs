//! Transitive closure reads over the stored citation edges.

mod common;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery_journal::{Batch, Closure, Digest, Journal, JournalError, OpaqueBytes, Ref, Seq, artifact_blob};
use aether_bloomery_kinds::{ClosureArtifact, ClosureLimit};
use aether_data::Kind;
use common::FixedClock;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.closure.branch")]
struct Branch {
    tag: u64,
    leaf: Ref<OpaqueBytes>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.closure.root")]
struct Root {
    branches: Vec<Ref<Branch>>,
}

/// A staged diamond: root → {A, B}, A → C, B → C.
struct Diamond {
    root: Digest,
    branches: [Digest; 2],
    leaf: Digest,
    total_bytes: u64,
}

impl Diamond {
    /// The documented order: the root, its children in ascending digest bytes, then the shared leaf once.
    fn expected_order(&self) -> Vec<Digest> {
        let mut branches = self.branches;
        branches.sort_by_key(|digest| *digest.as_bytes());
        vec![self.root, branches[0], branches[1], self.leaf]
    }
}

fn stage_diamond(journal: &mut Journal) -> Result<Diamond, Box<dyn Error>> {
    let mut batch = Batch::new();
    let leaf = batch.stage_bytes(b"shared leaf");
    let first = batch.stage_encoded(&Branch { tag: 1, leaf })?;
    let second = batch.stage_encoded(&Branch { tag: 2, leaf })?;
    let root = batch.stage_encoded(&Root { branches: vec![first, second] })?;

    let digests = [root.digest(), first.digest(), second.digest(), leaf.digest()];
    let mut total_bytes = 0;
    for digest in &digests {
        total_bytes += u64::try_from(batch.staged_blob(digest).ok_or("staged blob")?.len())?;
    }

    journal.append(Seq(0), &batch)?;
    Ok(Diamond { root: root.digest(), branches: [first.digest(), second.digest()], leaf: leaf.digest(), total_bytes })
}

fn found_digests(closure: Closure) -> Vec<Digest> {
    match closure {
        Closure::Found(artifacts) => artifacts.iter().map(ClosureArtifact::digest).collect(),
        other => panic!("expected Found, got {other:?}"),
    }
}

fn open(root: &Path) -> Result<Journal, JournalError> {
    Journal::open_with_clock(root, Box::new(FixedClock(0)))
}

fn blob_path(root: &Path, digest: &Digest) -> PathBuf {
    let hex = digest.to_string();
    root.join("blobs").join(&hex[..2]).join(hex)
}

#[test]
fn a_diamond_closure_is_root_first_breadth_first_and_deduplicated() -> Result<(), Box<dyn Error>> {
    // Catches a non-transitive walk, a shared leaf returned twice, and a nondeterministic child order.
    let (_root, mut journal) = common::temp_journal(0)?;
    let diamond = stage_diamond(&mut journal)?;

    let closure = journal.read_closure(&diamond.root, ClosureLimit::new(ClosureLimit::MAX_BYTES)?)?;
    assert_eq!(found_digests(closure), diamond.expected_order());
    Ok(())
}

#[test]
fn a_limit_equal_to_the_total_blob_length_fits_and_one_byte_less_does_not() -> Result<(), Box<dyn Error>> {
    // Catches an off-by-one at the budget and counting payload bytes instead of the prefixed blob length.
    let (_root, mut journal) = common::temp_journal(0)?;
    let diamond = stage_diamond(&mut journal)?;

    let exact = journal.read_closure(&diamond.root, ClosureLimit::new(diamond.total_bytes)?)?;
    assert_eq!(found_digests(exact), diamond.expected_order());
    let short = journal.read_closure(&diamond.root, ClosureLimit::new(diamond.total_bytes - 1)?)?;
    assert_eq!(short, Closure::TooLarge);
    Ok(())
}

#[test]
fn a_root_larger_than_the_limit_is_too_large_not_an_empty_list() -> Result<(), Box<dyn Error>> {
    // Catches a walk that leaves the root out of the budget or truncates to an empty or partial `Found`.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let root = batch.stage_bytes(&[9; 64]);
    journal.append(Seq(0), &batch)?;

    assert_eq!(journal.read_closure(&root.digest(), ClosureLimit::new(64)?)?, Closure::TooLarge);
    Ok(())
}

#[test]
fn an_unstored_root_is_missing() -> Result<(), Box<dyn Error>> {
    // Catches an absent root answered as an empty `Found`.
    let (_root, journal) = common::temp_journal(0)?;
    let absent = Digest::from_bytes([4; 32]);

    assert_eq!(journal.read_closure(&absent, ClosureLimit::new(ClosureLimit::MAX_BYTES)?)?, Closure::Missing(absent));
    Ok(())
}

#[test]
fn citation_edges_survive_a_reopen() -> Result<(), Box<dyn Error>> {
    // Catches edges kept only in memory, or DDL that drops or recreates the table on open.
    let temp = tempfile::tempdir()?;
    let diamond = stage_diamond(&mut open(temp.path())?)?;

    let closure = open(temp.path())?.read_closure(&diamond.root, ClosureLimit::new(ClosureLimit::MAX_BYTES)?)?;
    assert_eq!(found_digests(closure), diamond.expected_order());
    Ok(())
}

#[test]
fn a_member_whose_bytes_do_not_hash_to_its_digest_is_an_error() -> Result<(), Box<dyn Error>> {
    // Catches a walk that returns stored bytes without rechecking them against the digest key.
    let temp = tempfile::tempdir()?;
    let diamond = stage_diamond(&mut open(temp.path())?)?;

    // Same length as `shared leaf`, so only the hash check can catch it.
    fs::write(blob_path(temp.path(), &diamond.leaf), artifact_blob(OpaqueBytes::ID, b"forged leaf"))?;

    let error = open(temp.path())?
        .read_closure(&diamond.root, ClosureLimit::new(ClosureLimit::MAX_BYTES)?)
        .expect_err("forged member must fail");
    assert!(
        matches!(error, JournalError::ArtifactDigestMismatch(digest) if digest == diamond.leaf),
        "expected ArtifactDigestMismatch for the leaf, got {error:?}"
    );
    Ok(())
}
