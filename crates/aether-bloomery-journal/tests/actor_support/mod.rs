//! Actor-test harness shared by the journal actor tests: a root anchor, correlated request/reply
//! plumbing, and a probe that decodes blob-carrying replies against their attachments.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use aether_actor::{ActorRef, ErasedActorRef, actor};
use aether_bloomery_kinds::{ClosureArtifact, Digest, DigestMismatch, ReadArtifactResult, ReadClosureResult};
use aether_data::{Kind, KindId, Source, SourceAddr};
use aether_substrate::BootError;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::mail::MailRef;
use aether_substrate::mail::registry::{DispatchParts, MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::testing::registered_ref;

/// Mail the anchor accepts so it has a handler.
#[aether_data::kind(name = "test.bloomery.journal_actor.anchor_ping", default, no_serde)]
pub struct AnchorPing;

/// Root actor the test chassis boots before any journal actor is spawned.
pub struct TestAnchor {
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

/// Register a reply inbox named `name`; every reply it receives is forwarded to the receiver.
pub fn caller(registry: &Registry, name: &str) -> (ErasedActorRef, mpsc::Receiver<OwnedDispatch>) {
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

/// Enqueue `mail` on `target` as if `caller` sent it with `correlation`.
pub fn request<R, K: Kind>(
    registry: &Registry,
    target: ActorRef<R>,
    caller: ErasedActorRef,
    correlation: u64,
    mail: &K,
) {
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

/// Wait for the next reply, checking its kind and correlation.
pub fn reply<K: Kind>(rx: &mpsc::Receiver<OwnedDispatch>, correlation: u64) -> K {
    let dispatch = rx.recv_timeout(Duration::from_secs(2)).expect("reply within two seconds");
    assert_eq!(dispatch.kind, K::ID);
    assert_eq!(dispatch.sender.correlation_id, correlation);
    K::decode_from_bytes(dispatch.payload.bytes()).expect("decode reply")
}

/// One member as the probe read it: the digest its sender claims, its kind, and its payload
/// loaded against that claim.
#[derive(Debug, PartialEq, Eq)]
pub struct Member {
    pub digest: Digest,
    pub kind: KindId,
    pub payload: Result<Vec<u8>, DigestMismatch>,
}

impl Member {
    /// Read `artifact` through its one reader, against the digest it claims.
    fn read(artifact: &ClosureArtifact) -> Self {
        let digest = artifact.claimed().unverified();
        Self { digest, kind: artifact.kind(), payload: artifact.load(digest) }
    }
}

/// One reply the probe decoded, projected to plain values.
#[derive(Debug, PartialEq, Eq)]
pub enum Probed {
    /// A `ReadArtifactResult::Found`.
    Artifact(Member),
    /// A `ReadClosureResult::Found`: the echoed root and every member in order.
    Closure { root: Digest, members: Vec<Member> },
    /// Any other reply: the request was expected to be found.
    NotFound,
}

/// A decoded reply and the correlation it answers.
pub type Arrival = (u64, Probed);

/// Reply target for requests whose replies carry `Blob`s. Its typed handlers decode against the
/// mail's attachments, which the plain dispatch capture cannot, and forward a projection.
pub struct BlobProbe {
    arrivals: mpsc::Sender<Arrival>,
}

impl BlobProbe {
    fn forward<A>(&self, ctx: &NativeCtx<'_, A>, probed: Probed) {
        self.arrivals.send((ctx.reply_target().correlation_id, probed)).expect("the test holds the receiver");
    }
}

#[actor(instanced, root)]
impl NativeActor for BlobProbe {
    type Config = ();
    type Params = mpsc::Sender<Arrival>;
    const NAMESPACE: &'static str = "test.bloomery.journal_actor.blob_probe";

    fn init((): (), arrivals: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { arrivals })
    }

    #[handler::single]
    fn on_read_artifact_result(&mut self, ctx: &mut NativeCtx<'_>, result: ReadArtifactResult) {
        let probed = match result {
            ReadArtifactResult::Found { artifact } => Probed::Artifact(Member::read(&artifact)),
            ReadArtifactResult::Missing { .. } | ReadArtifactResult::Err { .. } => Probed::NotFound,
        };
        self.forward(ctx, probed);
    }

    #[handler::single]
    fn on_read_closure_result(&mut self, ctx: &mut NativeCtx<'_>, result: ReadClosureResult) {
        let probed = match result {
            ReadClosureResult::Found { root, artifacts } => {
                Probed::Closure { root, members: artifacts.iter().map(Member::read).collect() }
            }
            _ => Probed::NotFound,
        };
        self.forward(ctx, probed);
    }
}
