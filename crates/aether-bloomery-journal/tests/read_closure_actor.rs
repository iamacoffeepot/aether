//! A named journal owner answers closure reads with each of its replies.

mod actor_support;

use std::error::Error;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use aether_bloomery_journal::{Batch, Clock, Digest, Journal, JournalActor, OpaqueBytes, Ref, Seq};
use aether_bloomery_kinds::{ClosureLimit, ReadArtifact, ReadClosure, ReadClosureResult};
use aether_data::Kind;
use aether_substrate::Subname;
use aether_substrate::testing::{bare_substrate, boot_test_chassis_with};

use actor_support::{BlobProbe, Member, Probed, TestAnchor, caller, reply, request};

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

/// Stage root → leaf and return each member root-first, as a reader should find it, plus their
/// total blob length.
fn seed(journal_root: &Path) -> Result<(Vec<Member>, u64), Box<dyn Error>> {
    let mut batch = Batch::new();
    let leaf = batch.stage_bytes(b"closure leaf");
    let root = batch.stage_encoded(&Node { leaf })?;
    let mut members = Vec::new();
    let mut total_bytes = 0;
    for (digest, kind) in [(root.digest(), Node::ID), (leaf.digest(), OpaqueBytes::ID)] {
        let blob = batch.staged_blob(&digest).ok_or("staged blob")?;
        total_bytes += u64::try_from(blob.len())?;
        members.push(Member { digest, kind, payload: Ok(blob[8..].to_vec()) });
    }

    Journal::open_with_clock(journal_root, Box::new(FixedClock))?.append(Seq(0), &batch)?;
    Ok((members, total_bytes))
}

#[test]
fn read_closure_replies_found_too_large_and_missing() -> Result<(), Box<dyn Error>> {
    // Catches a missing handler, a `Closure` outcome mapped to the wrong reply variant or root, a
    // `Found` whose members were never checked in beside the reply (the probe's decode refuses a
    // detached blob), and a member checked in from the wrong slice (its payload or claim differs).
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("journal");
    let (members, total_bytes) = seed(&path)?;
    let root = members[0].digest;

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
    let (arrivals, probe_rx) = mpsc::channel();
    let probe =
        chassis.spawn_actor::<BlobProbe>(Subname::Named("closure_probe"), (), arrivals).finish().expect("probe birth");

    let found = ReadClosure { root, limit_bytes: ClosureLimit::new(total_bytes)? };
    request(&registry, journal, probe.erase(), 1, &found);
    let arrival = probe_rx.recv_timeout(Duration::from_secs(2)).expect("probe arrival within two seconds");
    assert_eq!(arrival, (1, Probed::Closure { root, members }));

    let short = ClosureLimit::new(total_bytes - 1)?;
    request(&registry, journal, reader, 2, &ReadClosure { root, limit_bytes: short });
    assert!(matches!(
        reply::<ReadClosureResult>(&rx, 2),
        ReadClosureResult::TooLarge { root: echoed, limit_bytes } if echoed == root && limit_bytes == short
    ));

    let absent = Digest::from_bytes([6; 32]);
    let generous = ClosureLimit::new(ClosureLimit::MAX_BYTES)?;
    request(&registry, journal, reader, 3, &ReadClosure { root: absent, limit_bytes: generous });
    assert!(matches!(
        reply::<ReadClosureResult>(&rx, 3),
        ReadClosureResult::Missing { root, digest } if root == absent && digest == absent
    ));
    Ok(())
}

/// Opens the FIFO's write end without blocking when dropped, so a run whose walk is still parked in
/// `File::open` on the read end gets released during unwinding instead of hanging teardown. With no
/// reader waiting the open fails, which is fine: nothing needs releasing.
#[cfg(unix)]
struct ReleaseFifo<'a>(&'a Path);

#[cfg(unix)]
impl Drop for ReleaseFifo<'_> {
    fn drop(&mut self) {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;

        drop(OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(self.0));
    }
}

#[cfg(unix)]
#[test]
fn a_blocked_closure_read_does_not_delay_a_read_artifact() -> Result<(), Box<dyn Error>> {
    // Catches a closure walk still running on the journal's dispatcher thread (the `ReadArtifact`
    // behind it is not answered while the walk blocks), and a closure reply parked across the
    // worker and then dropped (the walk's own caller never hears back once the walk finishes).
    use std::ffi::CString;
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir()?;
    let path = temp.path().join("journal");
    let (members, _) = seed(&path)?;
    let root = members[0].digest;
    let leaf_hex = members[1].digest.to_string();
    let fifo = path.join("blobs").join(&leaf_hex[..2]).join(&leaf_hex);
    fs::remove_file(&fifo)?;
    let fifo_name = CString::new(fifo.as_os_str().as_bytes())?;
    // SAFETY: `fifo_name` is a valid NUL-terminated path that outlives the call, and `mkfifo` only
    // reads it.
    let status = unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) };
    if status != 0 {
        return Err(io::Error::last_os_error().into());
    }

    let (registry, mailer) = bare_substrate();
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let journal = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("blocked"),
            (),
            Journal::open(&path).expect("open the journal root"),
        )
        .finish()
        .expect("journal birth");
    let (arrivals, probe_rx) = mpsc::channel();
    let probe =
        chassis.spawn_actor::<BlobProbe>(Subname::Named("blocked_probe"), (), arrivals).finish().expect("probe birth");

    // Declared after the chassis so that, unwinding, it releases a blocked walk before teardown.
    let release = ReleaseFifo(&fifo);

    // The walk reads the root into its slab region, then blocks opening the leaf's FIFO until a writer
    // opens it.
    let generous = ClosureLimit::new(ClosureLimit::MAX_BYTES)?;
    request(&registry, journal, probe.erase(), 1, &ReadClosure { root, limit_bytes: generous });
    request(&registry, journal, probe.erase(), 2, &ReadArtifact { digest: root });
    let root_member = members.into_iter().next().ok_or("root member")?;
    let arrival = probe_rx.recv_timeout(Duration::from_secs(2)).expect("artifact reply within two seconds");
    assert_eq!(arrival, (2, Probed::Artifact(root_member)));

    // A blocking open waits for the walk's reader; closing at once fails the leaf's length check.
    drop(OpenOptions::new().write(true).open(&fifo)?);
    let arrival = probe_rx.recv_timeout(Duration::from_secs(2)).expect("closure reply within two seconds");
    assert_eq!(arrival, (1, Probed::NotFound));
    drop(release);
    Ok(())
}
