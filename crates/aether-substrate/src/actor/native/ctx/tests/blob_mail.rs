//! In-process mail shares `Blob` fields through the engine store (ADR-0238
//! decision 3): a typed native send interns and attaches, a native handler's
//! decode resolves against the attachments, and only replies that stay in the
//! process carry tag 1.
//!
//! Every send runs in a turn of the booted [`Courier`], whose trigger kinds
//! each run one verb. Its mail lands either in a finishing sink, whose caught
//! envelope shows what was attached, or in a live pooled [`Keeper`], whose
//! own dispatch decodes it and whose values keep their entries until it
//! closes. The raw-forward tests have the courier mail itself a blob kind and
//! forward the bytes its next turn handles through a raw verb, which resolves
//! their tag-1 fields against that turn's attachments (resolve on send).

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use aether_actor::{ActorRef, Addressable, ErasedActorRef, HandlesKind};
use aether_data::{Blob, BlobReader, ErasedActorPath, Kind, KindDescriptor, MailId, Schema, SessionToken, Uuid};

use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Subname};
use crate::chassis::builder::{PassiveChassis, ReplyTarget};
use crate::chassis::error::BootError;
use crate::mail::EgressEvent;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::store::BlobStore;
use crate::testing::{PumpedDriver, TestChassis, boot_authority, boot_bare_test_chassis, fresh_substrate_and_rx};

use super::support::{CastOnly, sink};

const SHARED: &[u8] = b"bytes shared through the store";

#[aether_data::kind(name = "test.blob_mail.carrier")]
struct Carrier {
    blob: Blob,
}

#[aether_data::kind(name = "test.blob_mail.pair")]
struct Pair {
    first: Blob,
    second: Blob,
}

/// The kind only [`KeepsSetBlobs`] handles.
#[aether_data::kind(name = "test.blob_mail.set_carrier")]
struct SetCarrier {
    blob: Blob,
}

/// Asks a [`Keeper`] for a [`Carrier`] reply.
#[aether_data::kind(name = "test.blob_mail.ask", copy, default)]
struct Ask {
    tag: u32,
}

#[aether_data::kind(name = "test.blob_mail.note")]
struct Note {
    text: String,
}

/// Asks a [`Keeper`] for the bytes of every blob it keeps.
#[aether_data::kind(name = "test.blob_mail.report")]
struct Report;

/// A [`Keeper`]'s answer to [`Report`]: each kept blob's bytes, in arrival
/// order.
#[aether_data::kind(name = "test.blob_mail.kept")]
struct Kept {
    bytes: Vec<Vec<u8>>,
}

/// Asks a [`Keeper`] to shut down, dropping every value it keeps.
#[aether_data::kind(name = "test.blob_mail.leave")]
struct Leave;

/// A native handler set whose one arm keeps the blob it decodes.
#[aether_actor::handler_set]
trait KeepsSetBlobs {
    fn kept_blobs(&self) -> &Mutex<Vec<Blob>>;

    #[aether_actor::handler::single]
    fn on_set_carrier(&self, _ctx: &mut NativeCtx<'_>, mail: SetCarrier) {
        self.kept_blobs().lock().expect("kept blobs lock").push(mail.blob);
    }
}

/// A pooled peer that keeps every blob its arms decode, forwards each carried
/// blob to `forward` when set, and answers an [`Ask`] with a carried blob.
struct Keeper {
    kept: Mutex<Vec<Blob>>,
    forward: Option<ErasedActorRef>,
}

impl KeepsSetBlobs for Keeper {
    fn kept_blobs(&self) -> &Mutex<Vec<Blob>> {
        &self.kept
    }
}

#[aether_actor::actor(instanced, handler_set(KeepsSetBlobs))]
impl NativeActor for Keeper {
    const NAMESPACE: &'static str = "test.blob_mail.keeper";
    type Config = ();
    type Params = Option<ErasedActorRef>;

    fn init((): (), forward: Option<ErasedActorRef>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { kept: Mutex::new(Vec::new()), forward })
    }

    #[handler::single]
    fn on_carrier(&mut self, ctx: &mut NativeCtx<'_>, mail: Carrier) {
        if let Some(next) = self.forward {
            ctx.send_to(next, &Carrier { blob: mail.blob.clone() });
        }
        self.kept.lock().expect("kept blobs lock").push(mail.blob);
    }

    #[handler::single]
    fn on_pair(&mut self, _ctx: &mut NativeCtx<'_>, mail: Pair) {
        self.kept.lock().expect("kept blobs lock").extend([mail.first, mail.second]);
    }

    #[handler::single]
    fn on_ask(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Ask) -> Carrier {
        let _ = self;
        Carrier { blob: Blob::from(SHARED.to_vec()) }
    }

    #[handler::single]
    fn on_report(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Report) -> Kept {
        Kept { bytes: self.kept.lock().expect("kept blobs lock").iter().map(read).collect() }
    }

    #[handler::single]
    fn on_leave(&mut self, ctx: &mut NativeCtx<'_>, _mail: Leave) {
        let _ = self;
        ctx.shutdown();
    }
}

/// Sends a [`Carrier`] of the courier's blob to each recipient.
#[aether_data::kind(name = "test.blob_mail.send_carrier")]
struct SendCarrier;

/// Fans one [`Carrier`] of the courier's blob out to every recipient.
#[aether_data::kind(name = "test.blob_mail.fan_out")]
struct FanOut;

/// Sends the first recipient an [`Ask`].
#[aether_data::kind(name = "test.blob_mail.send_ask")]
struct SendAsk;

/// Answered with a [`Carrier`] reply.
#[aether_data::kind(name = "test.blob_mail.reply_carrier")]
struct ReplyCarrier;

/// Sends the first recipient a [`Note`], then a [`CastOnly`].
#[aether_data::kind(name = "test.blob_mail.send_blob_free")]
struct SendBlobFree;

/// Sends each recipient a [`SetCarrier`].
#[aether_data::kind(name = "test.blob_mail.send_set_carrier")]
struct SendSetCarrier;

/// Sends each recipient one [`Pair`] whose two fields hold equal bytes.
#[aether_data::kind(name = "test.blob_mail.send_pair")]
struct SendPair;

/// Sends the courier itself a [`Carrier`] of its blob.
#[aether_data::kind(name = "test.blob_mail.relay_carrier")]
struct RelayCarrier;

/// Sends the courier itself a [`Note`].
#[aether_data::kind(name = "test.blob_mail.relay_note")]
struct RelayNote;

/// A raw forward a [`Courier`] makes from each [`Carrier`] or [`Note`] turn.
struct RawForward {
    to: ErasedActorRef,
    /// The bytes it forwards as a [`Carrier`]: the handled payload's own when
    /// `None`.
    bytes: Option<Vec<u8>>,
}

/// What a [`Courier`] sends, and to whom.
#[derive(Default)]
struct Routes {
    /// The recipients of every typed send, in order.
    to: Vec<ErasedActorRef>,
    /// The blob its carriers carry: a fresh owned copy of [`SHARED`] when
    /// `None`.
    blob: Option<Blob>,
    raw: Option<RawForward>,
}

/// A pumped root that runs one send verb per trigger, keeps the blob each
/// [`Carrier`] it handles decodes to, and raw-forwards from those turns when
/// its routes say to.
struct Courier {
    routes: Routes,
    kept: Vec<Blob>,
    /// What each raw forward returned.
    forwarded: Vec<Option<MailId>>,
}

impl Courier {
    fn blob(&self) -> Blob {
        self.routes.blob.clone().unwrap_or_else(|| Blob::from(SHARED.to_vec()))
    }

    /// This courier's own proof, through the path its namespace names.
    fn me(ctx: &NativeCtx<'_, Self>) -> ErasedActorRef {
        ctx.resolve_path(&ErasedActorPath::new(Self::NAMESPACE).expect("a canonical path"))
            .expect("the courier is live")
    }

    /// Raw-forward the configured bytes, or this turn's own, as a
    /// [`Carrier`].
    fn forward_raw(&mut self, ctx: &mut NativeCtx<'_, Self>) {
        let Some(raw) = &self.routes.raw else {
            return;
        };
        let handled = ctx.inbound().expect("a handler turn has its inbound").payload.bytes().to_vec();
        let sent = ctx.send_envelope_tracked_to(raw.to, Carrier::ID, raw.bytes.as_deref().unwrap_or(&handled));
        self.forwarded.push(sent);
    }
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for Courier {
    const NAMESPACE: &'static str = "test.blob_mail.courier";
    type Config = ();
    type Params = Routes;

    fn init((): (), routes: Routes, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { routes, kept: Vec::new(), forwarded: Vec::new() })
    }

    #[handler::single]
    fn on_send_carrier(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendCarrier) {
        for &to in &self.routes.to {
            ctx.send_to(to, &Carrier { blob: self.blob() });
        }
    }

    #[handler::single]
    fn on_fan_out(&mut self, ctx: &mut NativeCtx<'_>, _trigger: FanOut) {
        ctx.fanout(self.routes.to.iter().copied(), &Carrier { blob: self.blob() });
    }

    #[handler::single]
    fn on_send_ask(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendAsk) {
        ctx.send_to(self.routes.to[0], &Ask { tag: 1 });
    }

    #[handler::single]
    fn on_reply_carrier(&mut self, _ctx: &mut NativeCtx<'_>, _trigger: ReplyCarrier) -> Carrier {
        Carrier { blob: self.blob() }
    }

    #[handler::single]
    fn on_send_blob_free(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendBlobFree) {
        ctx.send_to(self.routes.to[0], &Note { text: "no blobs here".into() });
        ctx.send_to(self.routes.to[0], &CastOnly { code: 0x6748 });
    }

    #[handler::single]
    fn on_send_set_carrier(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendSetCarrier) {
        for &to in &self.routes.to {
            ctx.send_to(to, &SetCarrier { blob: self.blob() });
        }
    }

    #[handler::single]
    fn on_send_pair(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendPair) {
        let pair = Pair { first: Blob::from(SHARED.to_vec()), second: Blob::from(SHARED.to_vec()) };
        for &to in &self.routes.to {
            ctx.send_to(to, &pair);
        }
    }

    #[handler::single]
    fn on_relay_carrier(&mut self, ctx: &mut NativeCtx<'_>, _trigger: RelayCarrier) {
        ctx.send_to(Self::me(ctx), &Carrier { blob: self.blob() });
    }

    #[handler::single]
    fn on_relay_note(&mut self, ctx: &mut NativeCtx<'_>, _trigger: RelayNote) {
        let _ = self;
        ctx.send_to(Self::me(ctx), &Note { text: "no blobs here".into() });
    }

    #[handler::single]
    fn on_carrier(&mut self, ctx: &mut NativeCtx<'_>, mail: Carrier) {
        self.forward_raw(ctx);
        self.kept.push(mail.blob);
    }

    #[handler::single]
    fn on_note(&mut self, ctx: &mut NativeCtx<'_>, _mail: Note) {
        self.forward_raw(ctx);
    }
}

/// Every byte of `blob`.
fn read(blob: &Blob) -> Vec<u8> {
    let mut reader = BlobReader::open(blob);
    let mut bytes = Vec::new();
    let mut buf = [0; 16];
    loop {
        let copied = reader.read(&mut buf);
        if copied == 0 {
            return bytes;
        }
        bytes.extend_from_slice(&buf[..copied]);
    }
}

/// A substrate whose [`Courier`] is booted once the peers and sinks its
/// [`Routes`] name stand, and the caller that reads the keepers' reports.
struct Rig {
    driver: PumpedDriver<Courier>,
    mailer: Arc<Mailer>,
    egress: Receiver<EgressEvent>,
    caller: (ErasedActorRef, Receiver<Envelope>),
}

/// The substrate a [`Rig`] boots on, before its courier: the place its peers
/// and sinks are stood.
struct Stage {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    egress: Receiver<EgressEvent>,
    chassis: PassiveChassis<TestChassis>,
}

impl Stage {
    fn new() -> Self {
        let (registry, mailer, egress) = fresh_substrate_and_rx();
        register_carrier(&registry);
        let chassis = boot_bare_test_chassis(&registry, &mailer);

        Self { registry, mailer, egress, chassis }
    }

    fn sink(&self, name: &str) -> (ErasedActorRef, Receiver<Envelope>) {
        sink(&self.registry, &self.mailer, name)
    }

    /// A [`Keeper`] spawned under `key`, forwarding each carrier to
    /// `forward`.
    fn keeper(&self, key: &str, forward: Option<ErasedActorRef>) -> ActorRef<Keeper> {
        self.chassis
            .spawn_actor_for_test::<Keeper>(Subname::Named(key), (), forward)
            .finish()
            .expect("the keeper spawns")
    }

    fn boot(self, routes: Routes) -> Rig {
        let caller = sink(&self.registry, &self.mailer, "test.blob_mail.caller");
        let driver = PumpedDriver::boot(self.chassis, (), routes);

        Rig { driver, mailer: self.mailer, egress: self.egress, caller }
    }
}

impl Rig {
    /// Push `trigger` to the courier as a chassis root answered to `reply`,
    /// and settle the chain every mail its turn sends joins.
    fn run<K: Kind>(&mut self, trigger: &K, reply: Option<ReplyTarget>)
    where
        Courier: HandlesKind<K>,
    {
        let courier = self.driver.chassis().actor_ref::<Courier>();
        self.driver.send_and_settle(courier, trigger, reply);
    }

    /// The bytes of every blob `keeper` keeps.
    fn kept(&mut self, keeper: ActorRef<Keeper>) -> Vec<Vec<u8>> {
        let reply = ReplyTarget::Actor { to: self.caller.0, correlation: 0 };
        self.driver.send_and_settle(keeper, &Report, Some(reply));
        let report = self.caller.1.try_recv().expect("the keeper answers its report");
        Kept::decode_from_bytes(report.payload.bytes()).expect("a report decodes").bytes
    }

    /// Close `keeper`, dropping every value it keeps.
    fn close(&mut self, keeper: ActorRef<Keeper>) {
        let _ = self.driver.send_tracked(keeper, &Leave, None);
        self.driver.chassis().await_closed(keeper.erase());
    }

    fn resident_bytes(&self) -> usize {
        self.mailer.blob_store().resident_bytes()
    }
}

/// (a) A native send of an `Owned` blob checks it into the store, and the
/// recipient's handler reads the same bytes from a value that alone keeps the
/// entry resident. Catches a send that never interns (the handler would hold
/// an `Owned` copy and nothing would be resident) or an entry that leaks past
/// its last holder.
#[test]
fn a_sent_owned_blob_is_shared_with_the_recipient_until_its_value_drops() {
    let stage = Stage::new();
    let b = stage.keeper("a-b", None);
    let mut rig = stage.boot(Routes { to: vec![b.erase()], ..Routes::default() });

    rig.run(&SendCarrier, None);

    assert_eq!(rig.kept(b), vec![SHARED.to_vec()]);
    assert_eq!(rig.resident_bytes(), SHARED.len(), "B's value keeps the one entry resident");
    rig.close(b);
    assert_eq!(rig.resident_bytes(), 0, "the entry goes with B's value");
}

/// (b) Re-sending a shared value attaches the entry it already holds: A sends
/// one, B forwards the value its handler decoded to C, and C forwards its own
/// to a sink. The entry lives in a second store, so a hop that copied the
/// bytes would check them into the engine's store, where check-in's dedup
/// cannot hide the copy, and the sink would catch an entry other than the
/// second store's. Catches a re-send that copies instead of attaching.
#[test]
fn a_resent_shared_blob_attaches_its_entry_without_copying() {
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let entry = foreign.check_in(Box::from(SHARED));
    let stage = Stage::new();
    let (d_ref, d_rx) = stage.sink("test.blob_mail.b.d");
    let c = stage.keeper("b-c", Some(d_ref));
    let b = stage.keeper("b-b", Some(c.erase()));
    let mut rig =
        stage.boot(Routes { to: vec![b.erase()], blob: Some(Arc::clone(&entry).into_blob()), ..Routes::default() });

    rig.run(&SendCarrier, None);

    let to_d = d_rx.try_recv().expect("C's forward reaches the sink");
    assert!(Arc::ptr_eq(&to_d.attachments()[0], &entry), "each hop attaches the entry its value holds");
    assert_eq!(rig.resident_bytes(), 0, "no hop checked the bytes in again");
    assert_eq!(rig.kept(b), vec![SHARED.to_vec()]);
    assert_eq!(rig.kept(c), vec![SHARED.to_vec()]);
}

/// (c) A fan-out gives each recipient its own strong reference: both read the
/// bytes, and the entry stays resident until the last recipient's value
/// drops. Catches a fan-out whose later recipients share, or miss, the first
/// one's reference.
#[test]
fn a_fanout_keeps_the_entry_until_every_recipient_drops_its_value() {
    let stage = Stage::new();
    let (b, c) = (stage.keeper("c-b", None), stage.keeper("c-c", None));
    let mut rig = stage.boot(Routes { to: vec![b.erase(), c.erase()], ..Routes::default() });

    rig.run(&FanOut, None);

    assert_eq!(rig.kept(b), vec![SHARED.to_vec()]);
    assert_eq!(rig.kept(c), vec![SHARED.to_vec()]);
    rig.close(b);
    assert_eq!(rig.resident_bytes(), SHARED.len(), "C's value still keeps the entry");
    assert_eq!(rig.kept(c), vec![SHARED.to_vec()]);
    rig.close(c);
    assert_eq!(rig.resident_bytes(), 0, "the entry goes with the last recipient's value");
}

/// (d) A reply to a component shares its blob: the requester's handler decodes
/// a value that alone keeps the entry. A reply to a session leaves the process,
/// so it reaches the hub as tag 0, which the plain decoder reads. Catches a
/// component reply left on the plain encoder, or a reply that leaves the
/// process carrying a tag-1 hash no far decoder can resolve.
#[test]
fn a_component_reply_shares_its_blob_and_a_session_reply_carries_bytes() {
    let stage = Stage::new();
    let keeper = stage.keeper("d-keeper", None);
    let mut rig = stage.boot(Routes { to: vec![keeper.erase()], ..Routes::default() });

    rig.run(&SendAsk, None);

    let requester_kept = rig.driver.read_state(|courier| courier.kept.iter().map(read).collect::<Vec<_>>());
    assert_eq!(requester_kept, Some(vec![SHARED.to_vec()]));
    assert_eq!(rig.resident_bytes(), SHARED.len(), "the decoded reply holds the entry");
    rig.driver.host_turn(|courier, _ctx| courier.kept.clear());

    let session = ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0x6748)), correlation: 1 };
    rig.run(&ReplyCarrier, Some(session));

    let payload = rig
        .egress
        .try_iter()
        .find_map(|event| match event {
            EgressEvent::ToSession { payload, .. } => Some(payload),
            _ => None,
        })
        .expect("the session reply reaches the hub");
    let reply = Carrier::decode_from_bytes(&payload).expect("the plain decoder reads a tag-0 session reply");
    assert_eq!(read(&reply.blob), SHARED);
    assert_eq!(rig.resident_bytes(), 0, "a session reply checks nothing into the store");
}

/// (e) Blob-free sends attach nothing, and their payload is exactly what the
/// plain encoder writes, for a structured kind and a cast kind alike. Catches
/// a send path that attaches without a `Blob` field, or whose encoding of
/// blob-free mail drifts from the wire bytes.
#[test]
fn a_blob_free_send_attaches_nothing_and_writes_the_plain_bytes() {
    let stage = Stage::new();
    let (b_ref, b_rx) = stage.sink("test.blob_mail.e.b");
    let mut rig = stage.boot(Routes { to: vec![b_ref], ..Routes::default() });

    rig.run(&SendBlobFree, None);

    let mut arrived: Vec<Envelope> = b_rx.try_iter().collect();
    arrived.sort_by_key(|envelope| envelope.mail_id.map(|id| id.correlation_id));
    let expected =
        [Note { text: "no blobs here".into() }.encode_into_bytes(), CastOnly { code: 0x6748 }.encode_into_bytes()];
    assert_eq!(arrived.len(), expected.len(), "each send reaches B");
    for (envelope, expected) in arrived.iter().zip(expected) {
        assert!(envelope.attachments().is_empty(), "a blob-free send attaches nothing");
        assert_eq!(envelope.payload.bytes(), expected, "a blob-free payload is the plain encoding");
    }
}

/// (f) A native handler-set arm decodes a blob kind against the attachments,
/// as the actor's own arms do. Catches a set arm left on the context-free
/// decode, which refuses every tag-1 field.
#[test]
fn a_handler_set_arm_decodes_a_shared_blob() {
    let stage = Stage::new();
    let b = stage.keeper("f-b", None);
    let mut rig = stage.boot(Routes { to: vec![b.erase()], ..Routes::default() });

    rig.run(&SendSetCarrier, None);

    assert_eq!(rig.kept(b), vec![SHARED.to_vec()]);
    assert_eq!(rig.resident_bytes(), SHARED.len(), "the set's value keeps the entry");
}

/// (g) Two fields holding equal bytes attach one entry, and both decode to
/// those bytes. Catches an entry attached once per field, or a resolver that
/// resolves only the first field naming a hash.
#[test]
fn two_fields_with_equal_bytes_attach_one_entry_both_resolve() {
    let stage = Stage::new();
    let (sink_ref, sink_rx) = stage.sink("test.blob_mail.g.sink");
    let b = stage.keeper("g-b", None);
    let mut rig = stage.boot(Routes { to: vec![sink_ref, b.erase()], ..Routes::default() });

    rig.run(&SendPair, None);

    let envelope = sink_rx.try_recv().expect("the send reaches the sink");
    assert_eq!(envelope.attachments().len(), 1, "equal bytes ride one attachment");
    assert_eq!(rig.kept(b), vec![SHARED.to_vec(), SHARED.to_vec()]);
}

/// Register [`Carrier`], so resolve on send has its schema to walk.
fn register_carrier(registry: &Registry) {
    registry
        .register_kind_with_descriptor(
            &boot_authority(),
            KindDescriptor { name: Carrier::NAME.into(), schema: Carrier::SCHEMA },
        )
        .expect("register the carrier kind");
}

/// A [`Carrier`] payload naming `hash`, as the envelope encoder writes one.
fn naming(hash: aether_data::BlobHash) -> Vec<u8> {
    let mut payload = vec![1];
    payload.extend_from_slice(hash.as_bytes());
    payload
}

/// What the courier's raw forwards returned.
fn forwarded(rig: &Rig) -> Vec<Option<MailId>> {
    rig.driver.read_state(|courier| courier.forwarded.clone()).expect("the courier is live")
}

/// (h) The courier, handling an attached mail, raw-forwards its bytes to C:
/// the forward attaches the entry the handled mail carried, C decodes a value
/// over it and forwards that value to a sink, and the engine's store does not
/// grow. The entry lives in a second store, so a forward that copied the
/// bytes in would show up there. Catches a raw forward that drops its
/// attachments, leaving C a hash it must refuse.
#[test]
fn a_raw_forward_of_attached_bytes_shares_their_entries() {
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let entry = foreign.check_in(Box::from(SHARED));
    let stage = Stage::new();
    let (d_ref, d_rx) = stage.sink("test.blob_mail.h.d");
    let c = stage.keeper("h-c", Some(d_ref));
    let mut rig = stage.boot(Routes {
        blob: Some(Arc::clone(&entry).into_blob()),
        raw: Some(RawForward { to: c.erase(), bytes: None }),
        ..Routes::default()
    });

    rig.run(&RelayCarrier, None);

    assert!(forwarded(&rig)[0].is_some(), "the forward is sent");
    assert_eq!(rig.kept(c), vec![SHARED.to_vec()], "C decodes the forwarded hash");
    let to_d = d_rx.try_recv().expect("C's forward reaches the sink");
    assert_eq!(to_d.attachments().len(), 1);
    assert!(Arc::ptr_eq(&to_d.attachments()[0], &entry), "C's value is the entry the handled mail carried");
    assert_eq!(rig.resident_bytes(), 0, "no hop checked the bytes into the engine store");
}

/// (i) The courier, handling an attached mail, raw-forwards bytes naming a
/// hash its mail does not carry: the verb refuses with `None` and nothing is
/// dispatched. Catches an unresolved hash leaving the sender, where no
/// recipient could resolve it and egress would let it out unrewritten.
#[test]
fn a_raw_forward_naming_a_hash_the_handled_mail_lacks_is_refused() {
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let other = foreign.check_in(Box::from(b"bytes B never received".as_slice()));
    let stage = Stage::new();
    let (c_ref, c_rx) = stage.sink("test.blob_mail.i.c");
    let mut rig = stage
        .boot(Routes { raw: Some(RawForward { to: c_ref, bytes: Some(naming(other.hash())) }), ..Routes::default() });

    rig.run(&RelayCarrier, None);

    assert!(forwarded(&rig)[0].is_none(), "the forward is refused at the sender");
    assert!(c_rx.try_recv().is_err(), "nothing is dispatched");
}

/// (j) A handler whose mail has no attachments holds no blob, so its raw
/// send of a tag-1 payload goes out unwalked and unattached, and the
/// recipient's decode refuses the detached hash. Pins ADR-0238 decision 3's
/// boundary: a change that starts walking blob-free senders (this send would
/// be refused) or starts attaching for them shows up here.
#[test]
fn a_raw_send_from_a_handler_holding_no_blob_goes_out_unwalked() {
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let entry = foreign.check_in(Box::from(SHARED));
    let stage = Stage::new();
    let (c_ref, c_rx) = stage.sink("test.blob_mail.j.c");
    let mut rig = stage
        .boot(Routes { raw: Some(RawForward { to: c_ref, bytes: Some(naming(entry.hash())) }), ..Routes::default() });

    rig.run(&RelayNote, None);

    assert!(forwarded(&rig)[0].is_some(), "a blob-free handler's raw send is not walked");
    let to_c = c_rx.try_recv().expect("the send reaches C");
    assert!(to_c.attachments().is_empty(), "nothing is attached");
    assert!(Carrier::decode_from_bytes(to_c.payload.bytes()).is_none(), "the recipient refuses the detached hash");
}
