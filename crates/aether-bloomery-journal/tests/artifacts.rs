//! Artifact put/get idempotence, tripwire digest, and positional batch contract.

mod common;

use std::error::Error;

use aether_bloomery_journal::Digest;
use common::journal;

const TRIPWIRE_PREIMAGE: &[u8] = b"aether-bloomery-journal";
const TRIPWIRE_DIGEST: [u8; 32] = [
    0xed, 0x93, 0x22, 0x6a, 0x36, 0x2b, 0x40, 0xa2, 0x9e, 0xb6, 0x6d, 0x81, 0xf5, 0xa8, 0x60, 0xaa, 0x50, 0x5c, 0xd1,
    0x65, 0xfc, 0x02, 0x48, 0x7d, 0x3e, 0x27, 0x93, 0x25, 0x94, 0x4e, 0xcd, 0x1c,
];

#[test]
fn putting_the_same_bytes_twice_returns_the_same_digest_and_different_bytes_differ() -> Result<(), Box<dyn Error>> {
    let mut journal = journal()?;
    let first = journal.put_artifact(b"alpha")?;
    let second = journal.put_artifact(b"alpha")?;
    assert_eq!(first, second);
    let other = journal.put_artifact(b"beta")?;
    assert_ne!(first, other);
    assert_eq!(journal.get_artifact(&first)?.as_deref(), Some(b"alpha".as_slice()));
    assert_eq!(journal.get_artifact(&other)?.as_deref(), Some(b"beta".as_slice()));
    Ok(())
}

#[test]
fn get_artifact_of_an_unknown_digest_is_none_and_a_put_digest_round_trips() -> Result<(), Box<dyn Error>> {
    let mut journal = journal()?;
    let missing = Digest([0; 32]);
    assert_eq!(journal.get_artifact(&missing)?, None);

    // Tripwire: pin sha256(b"aether-bloomery-journal"). Drifts if the hash or preimage changes.
    let digest = journal.put_artifact(TRIPWIRE_PREIMAGE)?;
    assert_eq!(digest.0, TRIPWIRE_DIGEST);
    assert_eq!(journal.get_artifact(&digest)?.as_deref(), Some(TRIPWIRE_PREIMAGE));
    Ok(())
}

#[test]
fn get_artifacts_keeps_absent_slots_and_put_artifacts_is_positional_and_idempotent() -> Result<(), Box<dyn Error>> {
    let mut journal = journal()?;
    let a = journal.put_artifact(b"present-a")?;
    let b = journal.put_artifact(b"present-b")?;
    let missing = Digest([1; 32]);

    let batch = journal.get_artifacts(&[a, missing, b])?;
    assert_eq!(batch.len(), 3);
    assert_eq!(batch[0].as_deref(), Some(b"present-a".as_slice()));
    assert_eq!(batch[1], None);
    assert_eq!(batch[2].as_deref(), Some(b"present-b".as_slice()));
    assert_eq!(batch[0], journal.get_artifact(&a)?);
    assert_eq!(batch[1], journal.get_artifact(&missing)?);
    assert_eq!(batch[2], journal.get_artifact(&b)?);

    let already = b"present-a";
    let fresh_one = b"fresh-1";
    let fresh_two = b"fresh-2";
    let digests = journal.put_artifacts(&[already.as_slice(), fresh_one.as_slice(), fresh_two.as_slice()])?;
    assert_eq!(digests.len(), 3);
    assert_eq!(digests[0], a);
    assert_ne!(digests[1], a);
    assert_ne!(digests[2], a);
    assert_ne!(digests[1], digests[2]);
    assert_eq!(journal.get_artifact(&digests[1])?.as_deref(), Some(fresh_one.as_slice()));
    assert_eq!(journal.get_artifact(&digests[2])?.as_deref(), Some(fresh_two.as_slice()));
    Ok(())
}
