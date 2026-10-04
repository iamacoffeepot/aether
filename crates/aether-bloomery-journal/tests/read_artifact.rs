//! Named journal owners return exact stored artifacts, answer a forged blob file for its receiver to
//! refuse, and refuse a short one.

mod actor_support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use aether_bloomery_journal::{Batch, Journal, JournalActor, JournalReader, ReadCacheBudget, Seq};
use aether_bloomery_kinds::{
    ArtifactDigests, ClosureLimit, DigestMismatch, Head, ReactorSet, ReadArtifact, ReadArtifactResult, ReadArtifacts,
    ReadArtifactsResult,
};
use aether_data::{Digest, Kind, KindId, OpaqueBytes, Storage, StorageData, Utf8Text, artifact_blob, artifact_digest};
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
            ReadCacheBudget::default(),
            Journal::open(&alpha_path).expect("open the journal root"),
        )
        .finish()
        .expect("alpha birth");
    let beta = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_beta"),
            ReadCacheBudget::default(),
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
            ReadCacheBudget::default(),
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
fn a_forged_payload_is_answered_for_its_receiver_to_refuse_and_a_short_prefix_is_an_error_without_journal_writes() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let mismatch_path = temp.path().join("mismatch");
    let short_path = temp.path().join("short");
    let mismatch = seed(&mismatch_path, b"original component").blob;
    let short = seed(&short_path, b"another component").blob;
    // Same length as the original, so the file passes the size check and only the receiver's digest check catches it.
    overwrite_blob(&mismatch_path, mismatch, &artifact_blob(OpaqueBytes::ID, b"modified component"));
    overwrite_blob(&short_path, short, b"short");

    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.corruption_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let (arrivals, probe_rx) = mpsc::channel();
    let probe = chassis
        .spawn_actor::<BlobProbe>(Subname::Named("corruption_probe"), (), arrivals)
        .finish()
        .expect("probe birth")
        .erase();
    let mismatch_owner = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_mismatch"),
            ReadCacheBudget::default(),
            Journal::open(&mismatch_path).expect("open the journal root"),
        )
        .finish()
        .expect("mismatch owner birth");
    let short_owner = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_short"),
            ReadCacheBudget::default(),
            Journal::open(&short_path).expect("open the journal root"),
        )
        .finish()
        .expect("short owner birth");

    request(&registry, mismatch_owner, probe, 81, &ReadArtifact { digest: mismatch });
    let forged = probed_by_correlation(&probe_rx, &[81]);
    let Some(Probed::Artifact(member)) = forged.get(&81) else {
        panic!("the forged payload is answered Found, got {forged:?}");
    };
    assert_eq!(member.digest, mismatch, "the answer claims the requested digest, so the receiver checks that key");
    assert_eq!(member.payload.as_ref().map_err(DigestMismatch::expected), Err(mismatch));

    request(&registry, short_owner, caller_id, 82, &ReadArtifact { digest: short });
    assert!(matches!(
        reply::<ReadArtifactResult>(&rx, 82),
        ReadArtifactResult::Err { digest, message } if digest == short && message.contains("shorter")
    ));
    assert_no_events(&mismatch_path);
    assert_no_events(&short_path);
}

#[test]
fn a_batched_read_answers_the_prefix_its_limit_covers_in_request_order() {
    // Catches an off-by-one at the cut, a reordered answer, a dropped first member under a small
    // limit, and a missing row that is not reported.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let path = temp.path().join("batched");
    let seeded = seed(&path, b"component");
    let absent = Digest::from_bytes([0; 32]);
    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.batched_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let (arrivals, probe_rx) = mpsc::channel();
    let probe =
        chassis.spawn_actor::<BlobProbe>(Subname::Named("batched_probe"), (), arrivals).finish().expect("probe birth");
    let owner = chassis
        .spawn_actor::<JournalActor>(
            Subname::Named("artifact_batched"),
            ReadCacheBudget::default(),
            Journal::open(&path).expect("open the journal root"),
        )
        .finish()
        .expect("birth");
    let read = |digests: Vec<Digest>, limit: u64| ReadArtifacts {
        digests: ArtifactDigests::new(digests).expect("a valid digest list"),
        limit_bytes: ClosureLimit::new(limit).expect("a valid limit"),
    };

    // Stored length is the payload plus the eight-byte kind prefix: "stored text" then "component".
    let first_two: u64 = (11 + 8) + (9 + 8);
    let order = vec![seeded.text, seeded.blob, seeded.empty];
    request(&registry, owner, probe.erase(), 91, &read(order.clone(), first_two));
    request(&registry, owner, probe.erase(), 92, &read(order, ClosureLimit::MIN_BYTES));
    let replies = probed_by_correlation(&probe_rx, &[91, 92]);
    let text = Member { digest: seeded.text, kind: Utf8Text::ID, payload: Ok(b"stored text".to_vec()) };
    let blob = Member { digest: seeded.blob, kind: OpaqueBytes::ID, payload: Ok(b"component".to_vec()) };
    assert_eq!(replies.get(&91), Some(&Probed::Artifacts(vec![text.clone(), blob])));
    assert_eq!(replies.get(&92), Some(&Probed::Artifacts(vec![text])));

    request(&registry, owner, caller_id, 93, &read(vec![seeded.text, absent, seeded.blob], first_two));
    assert!(
        matches!(reply::<ReadArtifactsResult>(&rx, 93), ReadArtifactsResult::Missing { digest } if digest == absent)
    );
    assert_no_events(&path);
}
