//! In-process mail shares `Blob` fields through the engine store (ADR-0238
//! decision 3): a typed native send interns and attaches, a native handler's
//! decode resolves against the attachments, and only replies that stay in the
//! process carry tag 1.
//!
//! Each send routes into an inbox sink, and the envelope it catches is then
//! dispatched to a [`Keeper`] through its `#[actor]` arms with the envelope
//! riding the ctx, as the native dispatcher does. The raw-forward tests hand
//! the caught envelope to a ctx as its inbound and forward its bytes through
//! a raw verb, which resolves their tag-1 fields against it (resolve on send).

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use aether_actor::{Erased, ErasedActorRef, Manual, OutboundReply};
use aether_data::{Blob, BlobReader, Kind, KindDescriptor, KindId, MailId, Schema, SessionToken, Uuid};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{Dispatch, NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::mail::registry::{OwnedDispatch, Registry};
use crate::mail::{EgressEvent, Source, SourceAddr};
use crate::store::BlobStore;
use crate::testing::{
    bare_substrate, boot_authority, registered_binding, registered_ref, test_mailer_and_rx, unrouted_binding,
};

use super::support::CastOnly;

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

/// A native handler set whose one arm keeps the blob it decodes.
#[aether_actor::handler_set]
trait KeepsSetBlobs {
    fn kept_blobs(&self) -> &Mutex<Vec<Blob>>;

    #[aether_actor::handler::single]
    fn on_set_carrier(&self, _ctx: &mut NativeCtx<'_>, mail: SetCarrier) {
        self.kept_blobs().lock().expect("kept blobs lock").push(mail.blob);
    }
}

/// Keeps every blob its arms decode, forwards each carried blob to `forward`
/// when set, and answers an [`Ask`] with a carried blob.
#[derive(Default)]
struct Keeper {
    kept: Mutex<Vec<Blob>>,
    forward: Option<ErasedActorRef>,
}

impl Keeper {
    /// The bytes of every kept blob, in arrival order.
    fn kept(&self) -> Vec<Vec<u8>> {
        self.kept.lock().expect("kept blobs lock").iter().map(read).collect()
    }
}

impl KeepsSetBlobs for Keeper {
    fn kept_blobs(&self) -> &Mutex<Vec<Blob>> {
        &self.kept
    }
}

#[aether_actor::actor(handler_set(KeepsSetBlobs))]
impl NativeActor for Keeper {
    const NAMESPACE: &'static str = "test.blob_mail.keeper";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
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

/// An inbox registered under `name` that hands each envelope it receives to
/// the returned receiver.
fn sink(registry: &Registry, name: &str) -> (ErasedActorRef, Receiver<Envelope>) {
    let (tx, rx) = mpsc::channel();
    let reference = registered_ref(
        registry,
        name,
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    (reference, rx)
}

/// Dispatch `envelope` to `keeper` through its `#[actor]` arms with the
/// envelope riding the ctx, as the native dispatcher does. `binding` is the
/// keeper's own, whose sends flush when the handler returns.
fn deliver(keeper: &mut Keeper, binding: &Arc<NativeBinding>, envelope: Envelope) {
    let kind = envelope.kind;
    let payload = envelope.payload.clone();
    let mut ctx = NativeCtx::<'_, Keeper, Manual>::with_inbound(
        binding,
        envelope.sender,
        envelope.mail_id,
        envelope.root,
        envelope,
    );

    let handled = <Keeper as Dispatch<Keeper>>::dispatch(keeper, &mut ctx, kind, payload.bytes());
    drop(ctx.take_raw_inbound());

    assert!(handled.is_some(), "an arm decodes and handles the mail");
}

/// (a) A native send of an `Owned` blob checks it into the store, and the
/// recipient's handler reads the same bytes from a value that alone keeps the
/// entry resident. Catches a send that never interns (the handler would hold
/// an `Owned` copy and nothing would be resident) or an entry that leaks past
/// its last holder.
#[test]
fn a_sent_owned_blob_is_shared_with_the_recipient_until_its_value_drops() {
    let (registry, mailer) = bare_substrate();
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.a.b");
    let sender = unrouted_binding(&mailer);

    {
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &Carrier { blob: Blob::from(SHARED.to_vec()) });
    }
    let mut b = Keeper::default();
    deliver(&mut b, &unrouted_binding(&mailer), b_rx.try_recv().expect("the send reaches B"));

    assert_eq!(b.kept(), vec![SHARED.to_vec()]);
    assert_eq!(mailer.blob_store().resident_bytes(), SHARED.len(), "B's value keeps the one entry resident");
    drop(b);
    assert_eq!(mailer.blob_store().resident_bytes(), 0, "the entry goes with B's value");
}

/// (b) Re-sending a shared value attaches the entry it already holds: A sends
/// one, and B forwards the value its handler decoded to C. The entry lives in
/// a second store, so a hop that copied the bytes would check them into the
/// engine's store, where check-in's dedup cannot hide the copy. Catches a
/// re-send that copies instead of attaching.
#[test]
fn a_resent_shared_blob_attaches_its_entry_without_copying() {
    let (registry, mailer) = bare_substrate();
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.b.b");
    let (c_ref, c_rx) = sink(&registry, "test.blob_mail.b.c");
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let entry = foreign.check_in(Box::from(SHARED));
    let sender = unrouted_binding(&mailer);

    {
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &Carrier { blob: Arc::clone(&entry).into_blob() });
    }
    let to_b = b_rx.try_recv().expect("the send reaches B");
    assert!(Arc::ptr_eq(&to_b.attachments()[0], &entry), "A's send attaches the entry its value holds");
    let mut b = Keeper { forward: Some(c_ref), ..Keeper::default() };
    deliver(&mut b, &unrouted_binding(&mailer), to_b);
    let to_c = c_rx.try_recv().expect("B's forward reaches C");

    assert!(Arc::ptr_eq(&to_c.attachments()[0], &entry), "B's forward attaches the entry its value holds");
    assert_eq!(mailer.blob_store().resident_bytes(), 0, "no hop checked the bytes in again");
    let mut c = Keeper::default();
    deliver(&mut c, &unrouted_binding(&mailer), to_c);
    assert_eq!(c.kept(), vec![SHARED.to_vec()]);
}

/// (c) A fan-out gives each recipient its own strong reference: both read the
/// bytes, and the entry stays resident until the last recipient's value
/// drops. Catches a fan-out whose later recipients share, or miss, the first
/// one's reference.
#[test]
fn a_fanout_keeps_the_entry_until_every_recipient_drops_its_value() {
    let (registry, mailer) = bare_substrate();
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.c.b");
    let (c_ref, c_rx) = sink(&registry, "test.blob_mail.c.c");
    let sender = unrouted_binding(&mailer);

    {
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.fanout([b_ref, c_ref], &Carrier { blob: Blob::from(SHARED.to_vec()) });
    }
    let mut b = Keeper::default();
    deliver(&mut b, &unrouted_binding(&mailer), b_rx.try_recv().expect("the fan-out reaches B"));
    let mut c = Keeper::default();
    deliver(&mut c, &unrouted_binding(&mailer), c_rx.try_recv().expect("the fan-out reaches C"));

    assert_eq!(b.kept(), vec![SHARED.to_vec()]);
    assert_eq!(c.kept(), vec![SHARED.to_vec()]);
    drop(b);
    assert_eq!(mailer.blob_store().resident_bytes(), SHARED.len(), "C's value still keeps the entry");
    assert_eq!(c.kept(), vec![SHARED.to_vec()]);
    drop(c);
    assert_eq!(mailer.blob_store().resident_bytes(), 0, "the entry goes with the last recipient's value");
}

/// (d) A reply to a component shares its blob: the requester's handler decodes
/// a value that alone keeps the entry. A reply to a session leaves the process,
/// so it reaches the hub as tag 0, which the plain decoder reads. Catches a
/// component reply left on the plain encoder, or a reply that leaves the
/// process carrying a tag-1 hash no far decoder can resolve.
#[test]
fn a_component_reply_shares_its_blob_and_a_session_reply_carries_bytes() {
    let (mailer, egress) = test_mailer_and_rx();
    let registry = mailer.registry();
    let (keeper_ref, keeper_rx) = sink(registry, "test.blob_mail.d.keeper");
    let (reply_tx, reply_rx) = mpsc::channel();
    let (requester, _) = registered_binding(
        registry,
        &mailer,
        "test.blob_mail.d.requester",
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = reply_tx.send(dispatch);
        }),
    );

    {
        let mut ctx = NativeCtx::new(&requester, Source::NONE, None, None);
        ctx.send_to(keeper_ref, &Ask { tag: 1 });
    }
    deliver(&mut Keeper::default(), &unrouted_binding(&mailer), keeper_rx.try_recv().expect("the ask arrives"));
    let mut requester_keeper = Keeper::default();
    deliver(&mut requester_keeper, &requester, reply_rx.try_recv().expect("the reply reaches the requester"));

    assert_eq!(requester_keeper.kept(), vec![SHARED.to_vec()]);
    assert_eq!(mailer.blob_store().resident_bytes(), SHARED.len(), "the decoded reply holds the entry");
    drop(requester_keeper);

    let session = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0x6748))));
    {
        let binding = unrouted_binding(&mailer);
        let mut ctx = NativeCtx::new_dispatching(&binding, session, None, None);
        OutboundReply::reply(&mut ctx, &Carrier { blob: Blob::from(SHARED.to_vec()) });
    }
    let payload = egress
        .try_iter()
        .find_map(|event| match event {
            EgressEvent::ToSession { payload, .. } => Some(payload),
            _ => None,
        })
        .expect("the session reply reaches the hub");
    let reply = Carrier::decode_from_bytes(&payload).expect("the plain decoder reads a tag-0 session reply");
    assert_eq!(read(&reply.blob), SHARED);
    assert_eq!(mailer.blob_store().resident_bytes(), 0, "a session reply checks nothing into the store");
}

/// (e) Blob-free sends attach nothing, and their payload is exactly what the
/// plain encoder writes, for a structured kind and a cast kind alike. Catches
/// a send path that attaches without a `Blob` field, or whose encoding of
/// blob-free mail drifts from the wire bytes.
#[test]
fn a_blob_free_send_attaches_nothing_and_writes_the_plain_bytes() {
    let (registry, mailer) = bare_substrate();
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.e.b");
    let sender = unrouted_binding(&mailer);
    let note = Note { text: "no blobs here".into() };
    let cast = CastOnly { code: 0x6748 };

    {
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &note);
        ctx.send_to(b_ref, &cast);
    }

    for expected in [note.encode_into_bytes(), cast.encode_into_bytes()] {
        let envelope = b_rx.try_recv().expect("each send reaches B");
        assert!(envelope.attachments().is_empty(), "a blob-free send attaches nothing");
        assert_eq!(envelope.payload.bytes(), expected, "a blob-free payload is the plain encoding");
    }
}

/// (f) A native handler-set arm decodes a blob kind against the attachments,
/// as the actor's own arms do. Catches a set arm left on the context-free
/// decode, which refuses every tag-1 field.
#[test]
fn a_handler_set_arm_decodes_a_shared_blob() {
    let (registry, mailer) = bare_substrate();
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.f.b");
    let sender = unrouted_binding(&mailer);

    {
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &SetCarrier { blob: Blob::from(SHARED.to_vec()) });
    }
    let mut b = Keeper::default();
    deliver(&mut b, &unrouted_binding(&mailer), b_rx.try_recv().expect("the send reaches B"));

    assert_eq!(b.kept(), vec![SHARED.to_vec()]);
    assert_eq!(mailer.blob_store().resident_bytes(), SHARED.len(), "the set's value keeps the entry");
}

/// (g) Two fields holding equal bytes attach one entry, and both decode to
/// those bytes. Catches an entry attached once per field, or a resolver that
/// resolves only the first field naming a hash.
#[test]
fn two_fields_with_equal_bytes_attach_one_entry_both_resolve() {
    let (registry, mailer) = bare_substrate();
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.g.b");
    let sender = unrouted_binding(&mailer);

    {
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &Pair { first: Blob::from(SHARED.to_vec()), second: Blob::from(SHARED.to_vec()) });
    }
    let envelope = b_rx.try_recv().expect("the send reaches B");
    assert_eq!(envelope.attachments().len(), 1, "equal bytes ride one attachment");
    let mut b = Keeper::default();
    deliver(&mut b, &unrouted_binding(&mailer), envelope);

    assert_eq!(b.kept(), vec![SHARED.to_vec(), SHARED.to_vec()]);
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

/// Handle `handled` on a ctx over `binding` and raw-forward `bytes` of `kind`
/// to `target` through `send_envelope_tracked_to`, returning what the verb
/// returned. The forward flushes when the ctx drops.
fn forward_raw(
    binding: &Arc<NativeBinding>,
    handled: Envelope,
    target: ErasedActorRef,
    kind: KindId,
    bytes: &[u8],
) -> Option<MailId> {
    let mut ctx =
        NativeCtx::<'_, Erased, Manual>::with_inbound(binding, handled.sender, handled.mail_id, handled.root, handled);
    let sent = ctx.send_envelope_tracked_to(target, kind, bytes);
    drop(ctx.take_raw_inbound());
    sent
}

/// A [`Carrier`] payload naming `hash`, as the envelope encoder writes one.
fn naming(hash: aether_data::BlobHash) -> Vec<u8> {
    let mut payload = vec![1];
    payload.extend_from_slice(hash.as_bytes());
    payload
}

/// (h) B, handling an attached mail, raw-forwards its bytes to C: the forward
/// attaches the entry B's mail carried, C decodes a value over it, and the
/// engine's store does not grow. The entry lives in a second store, so a
/// forward that copied the bytes in would show up there. Catches a raw
/// forward that drops its attachments, leaving C a hash it must refuse.
#[test]
fn a_raw_forward_of_attached_bytes_shares_their_entries() {
    let (registry, mailer) = bare_substrate();
    register_carrier(&registry);
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.h.b");
    let (c_ref, c_rx) = sink(&registry, "test.blob_mail.h.c");
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let entry = foreign.check_in(Box::from(SHARED));
    {
        let sender = unrouted_binding(&mailer);
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &Carrier { blob: Arc::clone(&entry).into_blob() });
    }
    let to_b = b_rx.try_recv().expect("the send reaches B");
    let bytes = to_b.payload.bytes().to_vec();

    let sent = forward_raw(&unrouted_binding(&mailer), to_b, c_ref, Carrier::ID, &bytes);

    assert!(sent.is_some(), "the forward is sent");
    let to_c = c_rx.try_recv().expect("the forward reaches C");
    assert_eq!(to_c.attachments().len(), 1);
    assert!(Arc::ptr_eq(&to_c.attachments()[0], &entry), "the forward attaches the entry B's mail carried");
    let mut c = Keeper::default();
    deliver(&mut c, &unrouted_binding(&mailer), to_c);
    assert_eq!(c.kept(), vec![SHARED.to_vec()]);
    assert_eq!(mailer.blob_store().resident_bytes(), 0, "no hop checked the bytes into the engine store");
}

/// (i) B, handling an attached mail, raw-forwards bytes naming a hash its
/// mail does not carry: the verb refuses with `None` and nothing is
/// dispatched. Catches an unresolved hash leaving the sender, where no
/// recipient could resolve it and egress would let it out unrewritten.
#[test]
fn a_raw_forward_naming_a_hash_the_handled_mail_lacks_is_refused() {
    let (registry, mailer) = bare_substrate();
    register_carrier(&registry);
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.i.b");
    let (c_ref, c_rx) = sink(&registry, "test.blob_mail.i.c");
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let other = foreign.check_in(Box::from(b"bytes B never received".as_slice()));
    {
        let sender = unrouted_binding(&mailer);
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &Carrier { blob: Blob::from(SHARED.to_vec()) });
    }
    let to_b = b_rx.try_recv().expect("the send reaches B");

    let sent = forward_raw(&unrouted_binding(&mailer), to_b, c_ref, Carrier::ID, &naming(other.hash()));

    assert!(sent.is_none(), "the forward is refused at the sender");
    assert!(c_rx.try_recv().is_err(), "nothing is dispatched");
}

/// (j) A handler whose mail has no attachments holds no blob, so its raw
/// send of a tag-1 payload goes out unwalked and unattached, and the
/// recipient's decode refuses the detached hash. Pins ADR-0238 decision 3's
/// boundary: a change that starts walking blob-free senders (this send would
/// be refused) or starts attaching for them shows up here.
#[test]
fn a_raw_send_from_a_handler_holding_no_blob_goes_out_unwalked() {
    let (registry, mailer) = bare_substrate();
    register_carrier(&registry);
    let (b_ref, b_rx) = sink(&registry, "test.blob_mail.j.b");
    let (c_ref, c_rx) = sink(&registry, "test.blob_mail.j.c");
    let foreign = BlobStore::new().expect("spawn the second store's reclaim thread");
    let entry = foreign.check_in(Box::from(SHARED));
    {
        let sender = unrouted_binding(&mailer);
        let mut ctx = NativeCtx::new(&sender, Source::NONE, None, None);
        ctx.send_to(b_ref, &Note { text: "no blobs here".into() });
    }
    let to_b = b_rx.try_recv().expect("the send reaches B");
    assert!(to_b.attachments().is_empty());

    let sent = forward_raw(&unrouted_binding(&mailer), to_b, c_ref, Carrier::ID, &naming(entry.hash()));

    assert!(sent.is_some(), "a blob-free handler's raw send is not walked");
    let to_c = c_rx.try_recv().expect("the send reaches C");
    assert!(to_c.attachments().is_empty(), "nothing is attached");
    assert!(Carrier::decode_from_bytes(to_c.payload.bytes()).is_none(), "the recipient refuses the detached hash");
}
