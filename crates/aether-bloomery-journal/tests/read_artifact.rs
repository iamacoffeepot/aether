//! Named journal owners return exact stored artifacts and refuse corrupt blob files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aether_actor::{ActorRef, ErasedActorRef, actor};
use aether_bloomery_journal::{Batch, Journal, JournalActor, JournalReader, Seq};
use aether_bloomery_kinds::{
    Digest, Head, OpaqueBytes, ReactorSet, ReadArtifact, ReadArtifactResult, Utf8Text, artifact_blob, artifact_digest,
};
use aether_data::{Kind, Source, SourceAddr, Storage, StorageData};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::mail::MailRef;
use aether_substrate::mail::registry::{DispatchParts, MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::testing::{bare_substrate, boot_test_chassis_with, registered_ref};
use aether_substrate::{BootError, Subname};

const CLUSTER: Head<OpaqueBytes> = Head::new("cluster.alpha");

#[aether_data::kind(name = "test.bloomery.journal_actor.anchor_ping", default, no_serde)]
struct AnchorPing;

struct TestAnchor {
    pings: u64,
}

#[actor(singleton, root)]
impl NativeActor for TestAnchor {
    type Config = ();
    const NAMESPACE: &'static str = "test.bloomery.journal_actor.anchor";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { pings: 0 })
    }

    #[handler::single]
    fn on_anchor_ping(&mut self, _ctx: &mut NativeCtx<'_>, _mail: AnchorPing) {
        self.pings += 1;
    }
}

fn caller(registry: &Registry, name: &str) -> (ErasedActorRef, mpsc::Receiver<OwnedDispatch>) {
    let (tx, rx) = mpsc::channel();
    let mailbox = registered_ref(
        registry,
        name,
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            tx.send(dispatch).expect("capture reply");
        }),
    );
    (mailbox, rx)
}

fn request<R, K: Kind>(registry: &Registry, target: ActorRef<R>, caller: ErasedActorRef, correlation: u64, mail: &K) {
    let target = target.erase();
    let MailboxEntry::Inbox { handler, .. } = registry.entry(target).expect("actor mailbox registered") else {
        panic!("actor mailbox is not an inbox");
    };
    handler.enqueue(OwnedDispatch::disarmed(
        DispatchParts {
            sender: Source::with_correlation(SourceAddr::Component(caller.id()), correlation),
            ..DispatchParts::new(K::ID, MailRef::from(mail.encode_into_bytes()))
        },
        target,
    ));
}

fn reply<K: Kind>(rx: &mpsc::Receiver<OwnedDispatch>, correlation: u64) -> K {
    let dispatch = rx.recv_timeout(Duration::from_secs(2)).expect("reply within two seconds");
    assert_eq!(dispatch.kind, K::ID);
    assert_eq!(dispatch.sender.correlation_id, correlation);
    K::decode_from_bytes(dispatch.payload.bytes()).expect("decode reply")
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
    let alpha = chassis
        .spawn_actor::<JournalActor>(Subname::Named("artifact_alpha"), alpha_path.clone(), ())
        .finish()
        .expect("alpha birth");
    let beta = chassis
        .spawn_actor::<JournalActor>(Subname::Named("artifact_beta"), beta_path.clone(), ())
        .finish()
        .expect("beta birth");
    assert_ne!(alpha, beta);

    request(&registry, alpha, caller_id, 11, &read);
    request(&registry, beta, caller_id, 22, &ReadArtifact { digest: beta_seed.blob });
    request(&registry, alpha, caller_id, 33, &ReadArtifact { digest: alpha_seed.empty });
    request(&registry, beta, caller_id, 44, &ReadArtifact { digest: beta_seed.set });
    let replies = replies_by_correlation(&rx, &[11, 22, 33, 44]);
    assert_eq!(
        replies.get(&11),
        Some(&ReadArtifactResult::Found {
            digest: alpha_seed.blob,
            kind: OpaqueBytes::ID,
            bytes: b"alpha component".to_vec()
        })
    );
    assert_eq!(
        replies.get(&22),
        Some(&ReadArtifactResult::Found {
            digest: beta_seed.blob,
            kind: OpaqueBytes::ID,
            bytes: b"beta component".to_vec()
        })
    );
    assert_eq!(
        replies.get(&33),
        Some(&ReadArtifactResult::Found { digest: alpha_seed.empty, kind: OpaqueBytes::ID, bytes: vec![] })
    );
    assert_eq!(
        replies.get(&44),
        Some(&ReadArtifactResult::Found { digest: beta_seed.set, kind: ReactorSet::ID, bytes: beta_seed.set_bytes })
    );

    request(&registry, beta, caller_id, 55, &ReadArtifact { digest: alpha_seed.blob });
    request(&registry, alpha, caller_id, 66, &ReadArtifact { digest: beta_seed.blob });
    let missing = replies_by_correlation(&rx, &[55, 66]);
    assert_eq!(missing.get(&55), Some(&ReadArtifactResult::Missing { digest: alpha_seed.blob }));
    assert_eq!(missing.get(&66), Some(&ReadArtifactResult::Missing { digest: beta_seed.blob }));
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
    let owner =
        chassis.spawn_actor::<JournalActor>(Subname::Named("artifact_text"), path.clone(), ()).finish().expect("birth");

    request(&registry, owner, caller_id, 71, &ReadArtifact { digest: seeded.text });
    assert_eq!(
        reply::<ReadArtifactResult>(&rx, 71),
        ReadArtifactResult::Found { digest: seeded.text, kind: Utf8Text::ID, bytes: b"stored text".to_vec() }
    );
    assert_ne!(Utf8Text::ID, OpaqueBytes::ID);
    assert_eq!(artifact_digest(Utf8Text::ID, b"stored text"), seeded.text);

    request(&registry, owner, caller_id, 72, &ReadArtifact { digest: absent });
    assert_eq!(reply::<ReadArtifactResult>(&rx, 72), ReadArtifactResult::Missing { digest: absent });
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
        .spawn_actor::<JournalActor>(Subname::Named("artifact_mismatch"), mismatch_path.clone(), ())
        .finish()
        .expect("mismatch owner birth");
    let short_owner = chassis
        .spawn_actor::<JournalActor>(Subname::Named("artifact_short"), short_path.clone(), ())
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
