//! A named journal owner answers closure reads with each of its replies.

mod actor_support;

use std::error::Error;
use std::path::Path;

use aether_bloomery_journal::{Batch, Clock, Digest, Journal, JournalActor, OpaqueBytes, Ref, Seq};
use aether_bloomery_kinds::{ClosureArtifact, ClosureLimit, ReadClosure, ReadClosureResult};
use aether_substrate::Subname;
use aether_substrate::testing::{bare_substrate, boot_test_chassis_with};

use actor_support::{TestAnchor, caller, reply, request};

struct FixedClock;

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        1_700_000_000_000
    }
}

#[derive(Clone, Debug, aether_data::Storage)]
#[kind(name = "test.bloomery.journal_actor.closure_node")]
struct Node {
    leaf: Ref<OpaqueBytes>,
}

/// Stage root → leaf and return the digests root-first plus their total blob length.
fn seed(journal_root: &Path) -> Result<(Vec<Digest>, u64), Box<dyn Error>> {
    let mut batch = Batch::new();
    let leaf = batch.stage_bytes(b"closure leaf");
    let root = batch.stage_encoded(&Node { leaf })?;
    let digests = vec![root.digest(), leaf.digest()];
    let mut total_bytes = 0;
    for digest in &digests {
        total_bytes += u64::try_from(batch.staged_blob(digest).ok_or("staged blob")?.len())?;
    }

    Journal::open_with_clock(journal_root, Box::new(FixedClock))?.append(Seq(0), &batch)?;
    Ok((digests, total_bytes))
}

#[test]
fn read_closure_replies_found_too_large_and_missing() -> Result<(), Box<dyn Error>> {
    // Catches a missing handler or a `Closure` outcome mapped to the wrong reply variant or root.
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("journal");
    let (digests, total_bytes) = seed(&path)?;
    let root = digests[0];

    let (registry, mailer) = bare_substrate();
    let (reader, rx) = caller(&registry, "test.journal_actor.closure_reader");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let journal = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("closure"),
            (),
            Journal::open(&path).expect("open the journal root"),
        )
        .finish()
        .expect("journal birth");

    request(&registry, journal, reader, 1, &ReadClosure { root, limit_bytes: ClosureLimit::new(total_bytes)? });
    let ReadClosureResult::Found { root: echoed, artifacts } = reply::<ReadClosureResult>(&rx, 1) else {
        panic!("expected Found");
    };
    assert_eq!(echoed, root);
    assert_eq!(artifacts.iter().map(ClosureArtifact::digest).collect::<Vec<_>>(), digests);

    let short = ClosureLimit::new(total_bytes - 1)?;
    request(&registry, journal, reader, 2, &ReadClosure { root, limit_bytes: short });
    assert_eq!(reply::<ReadClosureResult>(&rx, 2), ReadClosureResult::TooLarge { root, limit_bytes: short });

    let absent = Digest::from_bytes([6; 32]);
    let generous = ClosureLimit::new(ClosureLimit::MAX_BYTES)?;
    request(&registry, journal, reader, 3, &ReadClosure { root: absent, limit_bytes: generous });
    assert_eq!(reply::<ReadClosureResult>(&rx, 3), ReadClosureResult::Missing { root: absent, digest: absent });
    Ok(())
}
