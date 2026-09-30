//! A named journal owner answers `Stage`: content-addressed, unfenced, no event and no head move,
//! answered only once its rows commit, and refused whole when a citation dangles.

mod actor_support;

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::mpsc;
use std::time::Duration;

use aether_actor::{ProtocolRef, actor};
use aether_bloomery_journal::{Digest, Journal, JournalActor, OpaqueBytes, ReadCacheBudget, Ref};
use aether_bloomery_kinds::{
    ArtifactStorage, EncodedArtifact, Name, Node, ReadArtifact, ReadArtifactResult, ReadEvents, ReadEventsResult,
    ReadHead, ReadHeadResult, Stage, StageResult, Tree, artifact_digest,
};
use aether_data::{Kind, MAX_READ_BYTES};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::testing::{bare_substrate, boot_test_chassis_with};
use aether_substrate::{BootError, Subname};

use actor_support::{BlobProbe, Member, Probed, TestAnchor, caller, reply, request};

/// A tree holding one file, `leaf`.
fn tree_of(leaf: Ref<OpaqueBytes>) -> Tree {
    Tree::new(BTreeMap::from([(Name::new("leaf").expect("valid name"), Node::File(leaf))]))
}

/// `len` bytes that differ from their neighbours, so a dropped or repeated window changes them.
fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|index| u8::try_from(index % 251).expect("below 251")).collect()
}

/// Ask a [`Stager`] to check `len` patterned bytes in and stage them.
#[aether_data::kind(name = "test.bloomery.journal_actor.stage_checked_in", copy, no_serde)]
struct StageCheckedIn {
    len: u64,
}

/// Stages bytes it checked into the engine blob store through the journal's `ArtifactStorage`
/// rows, as a workspace does, and forwards each `StageResult` to the test.
struct Stager {
    storage: ProtocolRef<ArtifactStorage>,
    results: mpsc::Sender<StageResult>,
}

#[actor(instanced, root)]
impl NativeActor for Stager {
    type Config = ();
    type Params = (ProtocolRef<ArtifactStorage>, mpsc::Sender<StageResult>);
    const NAMESPACE: &'static str = "test.bloomery.journal_actor.stager";

    fn init((): (), (storage, results): Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { storage, results })
    }

    #[handler::tell]
    fn on_stage_checked_in(&mut self, ctx: &mut NativeCtx<'_>, mail: StageCheckedIn) {
        let len = usize::try_from(mail.len).expect("the test payload fits memory");
        let payload = ctx.check_in(patterned(len).into_boxed_slice());
        ctx.send_to(self.storage, &Stage::new(vec![EncodedArtifact::opaque_blob(payload)]));
    }

    #[handler::response]
    fn on_stage_result(&mut self, _ctx: &mut NativeCtx<'_>, result: StageResult) {
        self.results.send(result).expect("the test holds the receiver");
    }
}

#[test]
fn a_stage_stores_a_tree_and_its_blob_with_no_event_or_head_move() -> Result<(), Box<dyn Error>> {
    // Catches a stage that appends an event or moves the head, one answered before its rows
    // commit (the reads sent after the answer miss them), and one that refuses a tree citing a
    // blob staged in the same mail.
    let temp = tempfile::tempdir()?;
    let (registry, mailer) = bare_substrate();
    let (reader, rx) = caller(&registry, "test.journal_actor.stage_reader");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let journal = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("staging"),
            ReadCacheBudget::default(),
            Journal::open(&temp.path().join("journal"))?,
        )
        .finish()
        .expect("journal birth");
    let (arrivals, probe_rx) = mpsc::channel();
    let probe =
        chassis.spawn_actor::<BlobProbe>(Subname::Named("staging_probe"), (), arrivals).finish().expect("probe birth");

    let blob = EncodedArtifact::opaque_bytes(b"staged leaf");
    let leaf = Ref::<OpaqueBytes>::from_digest(blob.digest());
    let tree = EncodedArtifact::new(&tree_of(leaf))?;
    let tree_digest = tree.digest();
    request(&registry, journal, reader, 1, &Stage::new(vec![blob, tree]));
    assert_eq!(reply::<StageResult>(&rx, 1), StageResult::Staged);

    request(&registry, journal, reader, 2, &ReadHead);
    assert_eq!(reply::<ReadHeadResult>(&rx, 2), ReadHeadResult::Ok { head: 0 });
    request(&registry, journal, reader, 3, &ReadEvents { after: 0, limit: 8 });
    assert_eq!(reply::<ReadEventsResult>(&rx, 3), ReadEventsResult::Ok { after: 0, head: 0, entries: Vec::new() });

    for (correlation, digest) in [(4, leaf.digest()), (5, tree_digest)] {
        request(&registry, journal, probe.erase(), correlation, &ReadArtifact { digest });
        let (answered, probed) = probe_rx.recv_timeout(Duration::from_secs(2)).expect("artifact reply in two seconds");
        assert_eq!(answered, correlation);
        assert!(matches!(probed, Probed::Artifact(Member { digest: found, payload: Ok(_), .. }) if found == digest));
    }
    Ok(())
}

#[test]
fn a_dangling_citation_refuses_the_whole_stage() -> Result<(), Box<dyn Error>> {
    // Catches a partial commit: the blob staged beside a tree whose citation dangles is stored
    // anyway.
    let temp = tempfile::tempdir()?;
    let (registry, mailer) = bare_substrate();
    let (reader, rx) = caller(&registry, "test.journal_actor.dangling_reader");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let journal = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("dangling"),
            ReadCacheBudget::default(),
            Journal::open(&temp.path().join("journal"))?,
        )
        .finish()
        .expect("journal birth");

    let blob = EncodedArtifact::opaque_bytes(b"refused leaf");
    let blob_digest = blob.digest();
    let absent = Ref::<OpaqueBytes>::from_digest(Digest::from_bytes([9; 32]));
    let tree = EncodedArtifact::new(&tree_of(absent))?;
    request(&registry, journal, reader, 1, &Stage::new(vec![blob, tree]));
    assert!(matches!(reply::<StageResult>(&rx, 1), StageResult::Err { .. }));

    request(&registry, journal, reader, 2, &ReadArtifact { digest: blob_digest });
    assert!(matches!(
        reply::<ReadArtifactResult>(&rx, 2),
        ReadArtifactResult::Missing { digest } if digest == blob_digest
    ));
    Ok(())
}

#[test]
fn a_checked_in_payload_streams_whole_through_every_read_window() -> Result<(), Box<dyn Error>> {
    // Catches a streaming loop that drops or repeats the tail window: the payload is three whole
    // read windows plus one byte, and the stored row must read back and verify as those bytes.
    let temp = tempfile::tempdir()?;
    let (registry, mailer) = bare_substrate();
    let (sender, _unanswered) = caller(&registry, "test.journal_actor.stage_sender");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let journal = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("streaming"),
            ReadCacheBudget::default(),
            Journal::open(&temp.path().join("journal"))?,
        )
        .finish()
        .expect("journal birth");
    let (results, result_rx) = mpsc::channel();
    let stager = chassis
        .spawn_actor::<Stager>(Subname::Named("streaming_stager"), (), (journal.narrow::<ArtifactStorage>(), results))
        .finish()
        .expect("stager birth");
    let (arrivals, probe_rx) = mpsc::channel();
    let probe = chassis
        .spawn_actor::<BlobProbe>(Subname::Named("streaming_probe"), (), arrivals)
        .finish()
        .expect("probe birth");

    let payload = patterned(3 * MAX_READ_BYTES + 1);
    let digest = artifact_digest(OpaqueBytes::ID, &payload);
    request(&registry, stager, sender, 1, &StageCheckedIn { len: u64::try_from(payload.len())? });
    assert_eq!(result_rx.recv_timeout(Duration::from_secs(2))?, StageResult::Staged);

    request(&registry, journal, probe.erase(), 2, &ReadArtifact { digest });
    let arrival = probe_rx.recv_timeout(Duration::from_secs(2)).expect("artifact reply in two seconds");
    assert_eq!(arrival, (2, Probed::Artifact(Member { digest, kind: OpaqueBytes::ID, payload: Ok(payload) })));
    Ok(())
}
