//! Named journal owners return exact stored artifacts and refuse corrupt blob files.

mod actor_support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use aether_bloomery_journal::{Batch, Journal, JournalActor, JournalReader, Seq};
use aether_bloomery_kinds::{
    Digest, Head, OpaqueBytes, ReactorSet, ReadArtifact, ReadArtifactResult, Utf8Text, artifact_blob, artifact_digest,
};
use aether_data::{Kind, KindId, Storage, StorageData};
use aether_substrate::Subname;
use aether_substrate::mail::registry::OwnedDispatch;
use aether_substrate::testing::{bare_substrate, boot_test_chassis_with};

use actor_support::{BlobProbe, Member, Probed, TestAnchor, caller, reply, request};

const CLUSTER: Head<OpaqueBytes> = Head::new("cluster.alpha");

/// What a reader should find at `digest`: `payload` under `kind`, verified.
fn found(digest: Digest, kind: KindId, payload: &[u8]) -> Probed {
    Probed::Artifact(Member { digest, kind, payload: Ok(payload.to_vec()) })
}

/// Probe arrivals for `correlations`, keyed by correlation in any arrival order.
fn probed_by_correlation(rx: &mpsc::Receiver<actor_support::Arrival>, correlations: &[u64]) -> BTreeMap<u64, Probed> {
    let mut arrivals = BTreeMap::new();
    for _ in correlations {
        let (correlation, probed) = rx.recv_timeout(Duration::from_secs(2)).expect("probe arrival within two seconds");
        assert!(correlations.contains(&correlation), "unexpected correlation {correlation}");
        assert!(arrivals.insert(correlation, probed).is_none(), "duplicate correlation {correlation}");
    }
    arrivals
}

struct Seeded {
    blob: Digest,
    empty: Digest,
    text: Digest,
    set: Digest,
    set_bytes: Vec<u8>,
}

fn seed(root: &Path, blob: &[u8]) -> Seeded {
    let set = ReactorSet::new(vec![CLUSTER]).expect("one canonical reactor head");
    let set_bytes = ReactorSet::encode_storage(&StorageData::from_value(set.clone())).expect("encode set");
    let mut batch = Batch::new();
    let blob = batch.stage_bytes(blob).digest();
    let empty = batch.stage_bytes(b"").digest();
    let text = batch.stage_text("stored text").digest();
    let set = batch.stage_encoded(&set).expect("stage set").digest();
    Journal::open(root).expect("open seed journal").append(Seq(0), &batch).expect("commit artifacts");
    Seeded { blob, empty, text, set, set_bytes }
}

fn replies_by_correlation(
    rx: &mpsc::Receiver<OwnedDispatch>,
    correlations: &[u64],
) -> BTreeMap<u64, ReadArtifactResult> {
    let mut replies = BTreeMap::new();
    for _ in correlations {
        let dispatch = rx.recv_timeout(Duration::from_secs(2)).expect("reply within two seconds");
        assert_eq!(dispatch.kind, ReadArtifactResult::ID);
        let correlation = dispatch.sender.correlation_id;
        assert!(correlations.contains(&correlation), "unexpected correlation {correlation}");
        assert!(
            replies
                .insert(
                    correlation,
                    ReadArtifactResult::decode_from_bytes(dispatch.payload.bytes()).expect("decode artifact reply"),
                )
                .is_none(),
            "duplicate correlation {correlation}"
        );
    }
    replies
}

fn assert_no_events(root: &Path) {
    let journal = JournalReader::open(root).expect("inspect journal");
    assert_eq!(journal.head().expect("head"), Seq(0));
    assert!(journal.read(Seq(0), 1).expect("read events").is_empty());
}

fn blob_path(root: &Path, digest: &Digest) -> PathBuf {
    let hex = digest.to_string();
    root.join("blobs").join(&hex[..2]).join(hex)
}

fn overwrite_blob(root: &Path, digest: Digest, bytes: &[u8]) {
    let path = blob_path(root, &digest);
    assert!(path.is_file(), "the seeded blob file exists before it is overwritten");
    fs::write(path, bytes).expect("overwrite the blob file");
}

#[test]
fn named_owners_return_isolated_artifacts_with_order_independent_correlation() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let alpha_path = temp.path().join("alpha");
    let beta_path = temp.path().join("beta");
    let alpha_seed = seed(&alpha_path, b"alpha component");
    let beta_seed = seed(&beta_path, b"beta component");
    let read = ReadArtifact { digest: alpha_seed.blob };
    assert_eq!(ReadArtifact::decode_from_bytes(&read.encode_into_bytes()).expect("request round trip"), read);

    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.artifact_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let (arrivals, probe_rx) = mpsc::channel();
    let probe = chassis
        .spawn_actor::<BlobProbe>(Subname::Named("artifact_probe"), (), arrivals)
        .finish()
        .expect("probe birth")
        .erase();
    let alpha = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_alpha"),
            (),
            Journal::open(&alpha_path).expect("open the journal root"),
        )
        .finish()
        .expect("alpha birth");
    let beta = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_beta"),
            (),
            Journal::open(&beta_path).expect("open the journal root"),
        )
        .finish()
        .expect("beta birth");
    assert_ne!(alpha, beta);

    request(&registry, alpha, probe, 11, &read);
    request(&registry, beta, probe, 22, &ReadArtifact { digest: beta_seed.blob });
    request(&registry, alpha, probe, 33, &ReadArtifact { digest: alpha_seed.empty });
    request(&registry, beta, probe, 44, &ReadArtifact { digest: beta_seed.set });
    let replies = probed_by_correlation(&probe_rx, &[11, 22, 33, 44]);
    assert_eq!(replies.get(&11), Some(&found(alpha_seed.blob, OpaqueBytes::ID, b"alpha component")));
    assert_eq!(replies.get(&22), Some(&found(beta_seed.blob, OpaqueBytes::ID, b"beta component")));
    assert_eq!(replies.get(&33), Some(&found(alpha_seed.empty, OpaqueBytes::ID, b"")));
    assert_eq!(replies.get(&44), Some(&found(beta_seed.set, ReactorSet::ID, &beta_seed.set_bytes)));

    request(&registry, beta, caller_id, 55, &ReadArtifact { digest: alpha_seed.blob });
    request(&registry, alpha, caller_id, 66, &ReadArtifact { digest: beta_seed.blob });
    let missing = replies_by_correlation(&rx, &[55, 66]);
    assert!(matches!(missing.get(&55), Some(ReadArtifactResult::Missing { digest }) if *digest == alpha_seed.blob));
    assert!(matches!(missing.get(&66), Some(ReadArtifactResult::Missing { digest }) if *digest == beta_seed.blob));
    assert_no_events(&alpha_path);
    assert_no_events(&beta_path);
}

#[test]
fn text_kind_and_absent_digest_are_distinct_from_opaque_or_empty_bytes() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let path = temp.path().join("text");
    let seeded = seed(&path, b"component");
    let absent = Digest::from_bytes([0; 32]);
    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.text_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let (arrivals, probe_rx) = mpsc::channel();
    let probe =
        chassis.spawn_actor::<BlobProbe>(Subname::Named("text_probe"), (), arrivals).finish().expect("probe birth");
    let owner = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_text"),
            (),
            Journal::open(&path).expect("open the journal root"),
        )
        .finish()
        .expect("birth");

    request(&registry, owner, probe.erase(), 71, &ReadArtifact { digest: seeded.text });
    let text = probed_by_correlation(&probe_rx, &[71]);
    assert_eq!(text.get(&71), Some(&found(seeded.text, Utf8Text::ID, b"stored text")));
    assert_ne!(Utf8Text::ID, OpaqueBytes::ID);
    assert_eq!(artifact_digest(Utf8Text::ID, b"stored text"), seeded.text);

    request(&registry, owner, caller_id, 72, &ReadArtifact { digest: absent });
    assert!(matches!(reply::<ReadArtifactResult>(&rx, 72), ReadArtifactResult::Missing { digest } if digest == absent));
    assert_no_events(&path);
}

#[test]
fn changed_payload_and_short_prefix_are_errors_without_journal_writes() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let mismatch_path = temp.path().join("mismatch");
    let short_path = temp.path().join("short");
    let mismatch = seed(&mismatch_path, b"original component").blob;
    let short = seed(&short_path, b"another component").blob;
    // Same length as the original, so the file passes the size check and only the digest check catches it.
    overwrite_blob(&mismatch_path, mismatch, &artifact_blob(OpaqueBytes::ID, b"modified component"));
    overwrite_blob(&short_path, short, b"short");

    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.corruption_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let mismatch_owner = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_mismatch"),
            (),
            Journal::open(&mismatch_path).expect("open the journal root"),
        )
        .finish()
        .expect("mismatch owner birth");
    let short_owner = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_short"),
            (),
            Journal::open(&short_path).expect("open the journal root"),
        )
        .finish()
        .expect("short owner birth");
    request(&registry, mismatch_owner, caller_id, 81, &ReadArtifact { digest: mismatch });
    request(&registry, short_owner, caller_id, 82, &ReadArtifact { digest: short });
    let replies = replies_by_correlation(&rx, &[81, 82]);
    assert!(matches!(
        replies.get(&81),
        Some(ReadArtifactResult::Err { digest, message }) if *digest == mismatch && message.contains("digest")
    ));
    assert!(matches!(
        replies.get(&82),
        Some(ReadArtifactResult::Err { digest, message }) if *digest == short && message.contains("shorter")
    ));
    assert_no_events(&mismatch_path);
    assert_no_events(&short_path);
}
