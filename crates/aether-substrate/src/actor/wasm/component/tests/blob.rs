//! The guest blob table and host fns (`blob_hold_p32`, `blob_read_p32`,
//! `blob_drop_p32`, ADR-0238 decisions 2, 3, 4 and 9). Delivery tests drive
//! `Component::deliver` with an attached mail into a WAT guest whose
//! `receive_p32` acts on the hash at the start of its payload; the host fn
//! tests drive a WAT guest whose exports forward straight to the imports,
//! over entries pinned through `BlobTable::pin`. The resolve-on-send tests
//! drive a WAT guest that forwards, or replies with, the payload it received
//! through `send_mail_p32` / `reply_mail_p32`, as a guest raw-forwarding a
//! delivered `Blob` does. Entries come from a fresh `BlobStore`. The
//! `asset_blob_p32` tests (ADR-0163 §3) drive a WAT guest that carries an
//! asset section, takes it as a blob through an installed load window, and
//! forwards it.

use std::mem;
use std::sync::{Arc, Mutex};

use aether_data::{Blob, BlobReader, Kind, KindDescriptor, KindId, MAX_READ_BYTES, Schema, SessionToken, Uuid};
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store};

use super::{
    Component, DISPATCH_DROPPED_OVERSIZE, MAX_DELIVERABLE_MAIL_BYTES, WAT_HOOKS, WAT_REALLOC, ctx, ctx_at, inbound,
    instantiate, instantiate_with_ctx,
};
use crate::actor::native::BlobCheckIn;
use crate::actor::native::envelope::Envelope;
use crate::actor::wasm::ComponentCtx;
use crate::actor::wasm::asset_manifest::LoadWindow;
use crate::actor::wasm::host_fns::{
    self, ASSET_BLOB_NOT_FOUND, BLOB_NOT_HELD, BLOB_OUT_OF_BOUNDS, REPLY_OK, SEND_BLOB_REFUSED,
};
use crate::actor::wasm::module::ModuleCache;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::{OwnedDispatch, Registry};
use crate::mail::{MailboxId, Source, SourceAddr};
use crate::store::{BlobEntry, BlobStore};
use crate::testing::boot_authority;

/// A guest whose `hold` / `read` / `drop` exports forward their arguments to
/// the blob imports. 40 pages of memory (2.5 MiB) leave room for a
/// destination past `MAX_READ_BYTES`.
const WAT_BLOB_GUEST: &str = r#"
        (module
            (import "aether" "blob_hold_p32" (func $hold (param i32) (result i64)))
            (import "aether" "blob_read_p32" (func $read (param i32 i64 i32 i32) (result i64)))
            (import "aether" "blob_drop_p32" (func $drop (param i32)))
            (memory (export "memory") 40)
            (func (export "hold") (param i32) (result i64)
                local.get 0
                call $hold)
            (func (export "read") (param i32 i64 i32 i32) (result i64)
                local.get 0
                local.get 1
                local.get 2
                local.get 3
                call $read)
            (func (export "drop") (param i32)
                local.get 0
                call $drop))
    "#;

/// Where each test writes the 32-byte hash it names.
const HASH_AT: u32 = 16;

/// Where each small read lands.
const DST_AT: u32 = 64;

/// One instantiated [`WAT_BLOB_GUEST`] and its store, whose data carries the
/// instance's blob table.
struct Guest {
    store: Store<ComponentCtx>,
    instance: Instance,
    memory: Memory,
}

impl Guest {
    fn new() -> Self {
        let engine = Engine::default();
        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let module =
            Module::new(&engine, wat::parse_str(WAT_BLOB_GUEST).expect("compile WAT")).expect("compile module");

        let mut store = Store::new(&engine, ctx());
        let instance = linker.instantiate(&mut store, &module).expect("instantiate");
        let memory = instance.get_memory(&mut store, "memory").expect("memory export");
        Self { store, instance, memory }
    }

    /// Pin `entry` in this instance's table, as delivery does for the
    /// receive call. Nothing here unpins it until [`Self::unpin_all`].
    fn pin(&mut self, entry: &Arc<BlobEntry>) {
        self.store.data_mut().blob_table.pin(Arc::clone(entry));
    }

    fn unpin_all(&mut self) {
        self.store.data_mut().blob_table.unpin_all();
    }

    /// Write `entry`'s hash at [`HASH_AT`] and return that pointer.
    fn name(&mut self, entry: &BlobEntry) -> u32 {
        self.memory.write(&mut self.store, HASH_AT as usize, entry.hash().as_bytes()).expect("write hash");
        HASH_AT
    }

    fn bytes(&self, at: u32, len: usize) -> Vec<u8> {
        self.memory.data(&self.store)[at as usize..][..len].to_vec()
    }

    fn memory_len(&self) -> u32 {
        u32::try_from(self.memory.data_size(&self.store)).expect("guest memory fits the 32-bit ABI")
    }

    /// Call `blob_hold_p32`. A trap fails the test.
    fn hold(&mut self, hash_ptr: u32) -> i64 {
        let hold = self.instance.get_typed_func::<u32, i64>(&mut self.store, "hold").expect("hold export");
        hold.call(&mut self.store, hash_ptr).expect("blob_hold_p32 does not trap")
    }

    /// Call `blob_read_p32`. A trap fails the test.
    fn read(&mut self, hash_ptr: u32, offset: u64, dst_ptr: u32, dst_len: u32) -> i64 {
        let read =
            self.instance.get_typed_func::<(u32, u64, u32, u32), i64>(&mut self.store, "read").expect("read export");
        read.call(&mut self.store, (hash_ptr, offset, dst_ptr, dst_len)).expect("blob_read_p32 does not trap")
    }

    /// Call `blob_drop_p32`. A trap fails the test.
    fn drop_hold(&mut self, hash_ptr: u32) {
        let drop_hold = self.instance.get_typed_func::<u32, ()>(&mut self.store, "drop").expect("drop export");
        drop_hold.call(&mut self.store, hash_ptr).expect("blob_drop_p32 does not trap");
    }
}

/// The delivery guest's `receive_p32` acts on the kind id it is handed, over
/// the 32-byte hash at the start of its payload. `HOLD` stores
/// `blob_hold_p32`'s result at [`FIRST_HOLD_AT`]; `HOLD_TWICE` also stores a
/// second one at [`SECOND_HOLD_AT`], as a guest decoding one mail twice does;
/// `DROP` calls `blob_drop_p32`; `TRAP` traps; `IGNORE` does nothing, as a
/// guest that never decodes the field.
const HOLD: KindId = KindId(1);
const HOLD_TWICE: KindId = KindId(2);
const DROP: KindId = KindId(3);
const TRAP: KindId = KindId(4);
const IGNORE: KindId = KindId(5);

const FIRST_HOLD_AT: usize = 32;
const SECOND_HOLD_AT: usize = 40;

fn delivery_guest() -> Component {
    instantiate(&format!(
        r#"
        (module
            (import "aether" "blob_hold_p32" (func $hold (param i32) (result i64)))
            (import "aether" "blob_drop_p32" (func $drop (param i32)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32")
                (param $kind i64) (param $ptr i32) (param i32 i32 i32 i64 i64) (result i32)
                (if (i64.eq (local.get $kind) (i64.const {hold}))
                    (then (i64.store (i32.const {FIRST_HOLD_AT}) (call $hold (local.get $ptr)))))
                (if (i64.eq (local.get $kind) (i64.const {hold_twice}))
                    (then
                        (i64.store (i32.const {FIRST_HOLD_AT}) (call $hold (local.get $ptr)))
                        (i64.store (i32.const {SECOND_HOLD_AT}) (call $hold (local.get $ptr)))))
                (if (i64.eq (local.get $kind) (i64.const {drop}))
                    (then (call $drop (local.get $ptr))))
                (if (i64.eq (local.get $kind) (i64.const {trap}))
                    (then unreachable))
                i32.const 0))
    "#,
        hold = HOLD.0,
        hold_twice = HOLD_TWICE.0,
        drop = DROP.0,
        trap = TRAP.0,
    ))
}

/// A `kind` mail whose payload is `entry`'s hash, attaching nothing: the
/// shape of bytes a guest kept past the receive call that pinned them.
fn naming(kind: KindId, entry: &BlobEntry) -> Envelope {
    inbound(MailboxId(0), kind, entry.hash().as_bytes().to_vec(), Source::NONE)
}

/// A `kind` mail whose payload is `entry`'s hash and whose envelope attaches
/// `entry`, as an in-process send of a `Blob` field builds one.
fn attaching(kind: KindId, entry: &Arc<BlobEntry>) -> Envelope {
    naming(kind, entry).with_attachments(Some(vec![Arc::clone(entry)].into_boxed_slice()))
}

/// The `i64` the delivery guest stored at `at`.
fn stored(component: &mut Component, at: usize) -> i64 {
    i64::from_le_bytes(component.read_bytes(at, 8).try_into().expect("eight bytes"))
}

fn store() -> BlobStore {
    BlobStore::new().expect("spawn the reclaim thread")
}

/// `len` bytes that differ at every offset a test reads from.
fn patterned(len: usize) -> Box<[u8]> {
    (0..=250).cycle().take(len).collect()
}

/// Catches an attached hash that does not resolve during the receive call (a
/// pin missing, or taken after the call), and a hold that does not outlive
/// the call or counts more than once.
#[test]
fn a_hold_taken_during_an_attached_receive_keeps_the_entry_after_it() {
    let store = store();
    let entry = store.check_in(patterned(40));
    let mut guest = delivery_guest();

    guest.deliver(&attaching(HOLD, &entry)).expect("deliver");

    assert_eq!(stored(&mut guest, FIRST_HOLD_AT), 40, "the hold returns the entry's length");

    let mail = naming(DROP, &entry);
    drop(entry);
    assert_eq!(store.resident_bytes(), 40, "the guest's hold keeps the entry");

    guest.deliver(&mail).expect("deliver");

    assert_eq!(store.resident_bytes(), 0, "one drop gives back the one hold");
}

/// Catches pins that outlive the receive call: a field the guest never
/// decodes must hold nothing once `receive_p32` returns.
#[test]
fn a_field_never_decoded_holds_nothing_after_the_call() {
    let store = store();
    let entry = store.check_in(patterned(24));
    let mut guest = delivery_guest();

    guest.deliver(&attaching(IGNORE, &entry)).expect("deliver");
    let hash = entry.hash();
    drop(entry);

    assert!(guest.store.data().blob_table.entry(hash).is_none());
    assert_eq!(store.resident_bytes(), 0);
}

/// Catches admission after the pin has gone: a hash kept from an earlier
/// receive is refused in a later one that attaches nothing.
#[test]
fn a_hash_kept_past_its_receive_is_refused_later() {
    let store = store();
    let entry = store.check_in(patterned(24));
    let mut guest = delivery_guest();

    guest.deliver(&attaching(IGNORE, &entry)).expect("deliver");
    guest.deliver(&naming(HOLD, &entry)).expect("a refused hold does not trap");

    assert_eq!(stored(&mut guest, FIRST_HOLD_AT), BLOB_NOT_HELD);
}

/// Catches counts that drift from live values: decoding one mail twice holds
/// twice, the second drop (not the first) frees the entry, and a third drop
/// warns without trapping or disturbing anything.
#[test]
fn decoding_a_mail_twice_holds_twice_and_each_drop_gives_one_back() {
    let store = store();
    let entry = store.check_in(patterned(32));
    let mut guest = delivery_guest();

    guest.deliver(&attaching(HOLD_TWICE, &entry)).expect("deliver");

    assert_eq!(stored(&mut guest, FIRST_HOLD_AT), 32);
    assert_eq!(stored(&mut guest, SECOND_HOLD_AT), 32);

    let drop_mail = naming(DROP, &entry);
    drop(entry);
    guest.deliver(&drop_mail).expect("deliver");

    assert_eq!(store.resident_bytes(), 32, "one hold remains");

    guest.deliver(&drop_mail).expect("deliver");

    assert_eq!(store.resident_bytes(), 0);

    guest.deliver(&drop_mail).expect("a drop with no hold does not trap");
}

/// Catches a pin on a dropped delivery: an attached mail past the deliverable
/// ceiling never reaches `receive_p32` and leaves nothing in the table.
#[test]
fn an_oversize_attached_delivery_pins_nothing() {
    let store = store();
    let entry = store.check_in(patterned(16));
    let mut guest = delivery_guest();
    let mail = inbound(MailboxId(0), HOLD, vec![0; MAX_DELIVERABLE_MAIL_BYTES + 1], Source::NONE)
        .with_attachments(Some(vec![Arc::clone(&entry)].into_boxed_slice()));

    assert_eq!(guest.deliver(&mail).expect("an oversize drop does not trap"), DISPATCH_DROPPED_OVERSIZE);

    assert!(guest.store.data().blob_table.entry(entry.hash()).is_none());
}

/// Catches the `Err` path skipping `unpin_all`: a receive that traps must
/// still leave its attachments unpinned, so the table admits no later hold.
#[test]
fn a_trapping_receive_still_unpins() {
    let store = store();
    let entry = store.check_in(patterned(16));
    let mut guest = delivery_guest();

    guest.deliver(&attaching(TRAP, &entry)).expect_err("the guest traps");
    let hash = entry.hash();
    drop(entry);

    assert!(guest.store.data().blob_table.entry(hash).is_none());
    assert_eq!(store.resident_bytes(), 0);
}

/// Catches a wrong slice: reading from the wrong offset, past the requested
/// length, or past the blob's end.
#[test]
fn hold_and_read_at_an_offset_copy_the_right_bytes() {
    let store = store();
    let entry = store.check_in(patterned(64));
    let mut guest = Guest::new();
    guest.pin(&entry);
    let hash = guest.name(&entry);

    assert_eq!(guest.hold(hash), 64);

    assert_eq!(guest.read(hash, 10, DST_AT, 8), 8);
    assert_eq!(guest.bytes(DST_AT, 8), entry.bytes()[10..18]);

    assert_eq!(guest.read(hash, 60, DST_AT, 8), 4, "a read near the end copies only what is left");
    assert_eq!(guest.bytes(DST_AT, 4), entry.bytes()[60..]);

    assert_eq!(guest.read(hash, 64, DST_AT, 8), 0, "a read at the end copies nothing");
}

/// Catches a lookup outside the caller's own table (a hash resident in the
/// store, and held by another instance, must still be refused here), and a
/// trap on an unheld hash.
#[test]
fn an_unheld_hash_is_refused_without_trapping() {
    let store = store();
    let held = store.check_in(patterned(16));
    let elsewhere = store.check_in(b"held by another instance".as_slice().into());
    let mut other = Guest::new();
    other.pin(&elsewhere);
    let other_hash = other.name(&elsewhere);
    assert_eq!(other.hold(other_hash), 24);
    other.unpin_all();
    let mut guest = Guest::new();
    guest.pin(&held);

    let hash = guest.name(&elsewhere);

    assert_eq!(guest.hold(hash), BLOB_NOT_HELD);
    assert_eq!(guest.read(hash, 0, DST_AT, 8), BLOB_NOT_HELD);
    assert_eq!(guest.bytes(DST_AT, 8), [0; 8], "a refused read writes nothing");

    guest.drop_hold(hash);
    assert_eq!(other.hold(other_hash), 24, "a refused drop leaves the holder's hold alone");
}

/// Catches an unchecked copy: a hash or destination range that runs past the
/// end of guest memory must be refused with nothing written.
#[test]
fn out_of_bounds_pointers_are_refused_and_write_nothing() {
    let store = store();
    let entry = store.check_in(patterned(16));
    let mut guest = Guest::new();
    guest.pin(&entry);
    let hash = guest.name(&entry);
    assert_eq!(guest.hold(hash), 16);
    guest.unpin_all();
    let end = guest.memory_len();

    assert_eq!(guest.hold(end - 16), BLOB_OUT_OF_BOUNDS, "a hash straddling the end of memory");
    assert_eq!(guest.read(end - 16, 0, DST_AT, 8), BLOB_OUT_OF_BOUNDS);

    assert_eq!(guest.read(hash, 0, end - 4, 8), BLOB_OUT_OF_BOUNDS, "a destination straddling the end");
    assert_eq!(guest.bytes(end - 4, 4), [0; 4]);

    guest.drop_hold(end - 16);
    assert_eq!(guest.read(hash, 0, DST_AT, 8), 8, "an out-of-bounds drop releases nothing");
}

/// Catches a missing host clamp: a destination larger than `MAX_READ_BYTES`
/// receives exactly `MAX_READ_BYTES`, and nothing past them.
#[test]
fn a_read_copies_at_most_max_read_bytes() {
    let store = store();
    let entry = store.check_in(patterned(MAX_READ_BYTES + 100));
    let mut guest = Guest::new();
    guest.pin(&entry);
    let hash = guest.name(&entry);
    let dst = 4096;
    let window = u32::try_from(MAX_READ_BYTES + 100).expect("the window fits the 32-bit ABI");

    assert_eq!(guest.read(hash, 0, dst, window), i64::try_from(MAX_READ_BYTES).expect("fits"));
    assert_eq!(guest.bytes(dst, MAX_READ_BYTES), entry.bytes()[..MAX_READ_BYTES]);
    assert_eq!(guest.bytes(dst + u32::try_from(MAX_READ_BYTES).expect("fits"), 100), [0; 100]);
}

/// Catches a teardown that keeps entries: dropping a `Component` whose table
/// still holds an entry must release it.
#[test]
fn dropping_a_component_releases_what_its_table_still_holds() {
    let store = store();
    let entry = store.check_in(patterned(48));
    let mut component = instantiate(WAT_HOOKS);
    let table = &mut component.store.data_mut().blob_table;
    table.pin(Arc::clone(&entry));
    table.hold(entry.hash()).expect("pinned");
    table.hold(entry.hash()).expect("pinned");
    table.unpin_all();
    drop(entry);

    assert_eq!(store.resident_bytes(), 48);

    drop(component);

    assert_eq!(store.resident_bytes(), 0);
}

/// A registered kind with one `Blob` field, so resolve on send has a schema
/// to walk. Its wire form is the field alone: tag 1 and a hash, or tag 0,
/// a length and the bytes.
#[aether_data::kind(name = "test.guest_blob.carrier")]
struct GuestCarrier {
    blob: Blob,
}

/// Where the resolve-on-send guests store the host fn's status. The data
/// segment starts it at `u32::MAX`, so a guest that never made the call
/// cannot read as a success.
const STATUS_AT: usize = 500;

/// A guest that forwards every payload it receives to `recipient` as a
/// [`GuestCarrier`] through `send_mail_p32`, storing the status at
/// [`STATUS_AT`].
fn forwarding_guest(recipient: MailboxId) -> String {
    format!(
        r#"
        (module
            (import "aether" "send_mail_p32"
                (func $send (param i64 i64 i32 i32 i32 i32 i64) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const {STATUS_AT}) "\ff\ff\ff\ff")
            {WAT_REALLOC}
            (func (export "receive_p32")
                (param i64) (param $ptr i32) (param $len i32) (param i32 i32 i64 i64) (result i32)
                (i32.store (i32.const {STATUS_AT}) (call $send
                    (i64.const {recipient})
                    (i64.const {carrier})
                    (local.get $ptr)
                    (local.get $len)
                    (i32.const 1)
                    (i32.const 0)
                    (i64.const 0)))
                i32.const 0))
        "#,
        recipient = recipient.0,
        carrier = GuestCarrier::ID.0,
    )
}

/// A guest that takes a hold on the hash its payload names (the payload is a
/// tag-1 [`GuestCarrier`], so the hash starts one byte in), then replies with
/// the payload as a [`GuestCarrier`] through `reply_mail_p32`, storing the
/// status at [`STATUS_AT`].
fn replying_guest() -> String {
    format!(
        r#"
        (module
            (import "aether" "blob_hold_p32" (func $hold (param i32) (result i64)))
            (import "aether" "reply_mail_p32"
                (func $reply (param i32 i64 i32 i32 i32 i64) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const {STATUS_AT}) "\ff\ff\ff\ff")
            {WAT_REALLOC}
            (func (export "receive_p32")
                (param i64) (param $ptr i32) (param $len i32) (param i32) (param $sender i32) (param i64 i64)
                (result i32)
                (drop (call $hold (i32.add (local.get $ptr) (i32.const 1))))
                (i32.store (i32.const {STATUS_AT}) (call $reply
                    (local.get $sender)
                    (i64.const {carrier})
                    (local.get $ptr)
                    (local.get $len)
                    (i32.const 1)
                    (i64.const 0)))
                i32.const 0))
        "#,
        carrier = GuestCarrier::ID.0,
    )
}

/// A [`GuestCarrier`] payload naming `entry` by hash, as a guest's encode of
/// a held value writes it.
fn tagged(entry: &BlobEntry) -> Vec<u8> {
    let mut payload = vec![1];
    payload.extend_from_slice(entry.hash().as_bytes());
    payload
}

/// A registry that knows [`GuestCarrier`].
fn carrier_registry() -> Arc<Registry> {
    let registry = Arc::new(Registry::new());
    registry
        .register_kind_with_descriptor(
            &boot_authority(),
            KindDescriptor { name: GuestCarrier::NAME.into(), schema: GuestCarrier::SCHEMA },
        )
        .expect("register the carrier kind");
    registry
}

/// Every dispatch a [`recipient_sink`] received.
type Received = Arc<Mutex<Vec<Envelope>>>;

/// A recipient registered in `registry` that keeps every dispatch it
/// receives.
fn recipient_sink(registry: &Registry) -> (MailboxId, Received) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    let recipient = registry
        .try_register_inbox(
            &boot_authority(),
            "test.guest_blob.recipient",
            Arc::new(move |dispatch: OwnedDispatch| {
                // Terminal test sink (ADR-0094): discharge before keeping.
                dispatch.discharge();
                sink.lock().expect("sink lock").push(dispatch);
            }),
        )
        .expect("register the recipient");
    (recipient, received)
}

/// A [`forwarding_guest`] whose recipient is a sink that keeps every
/// dispatch it receives.
struct Forwarder {
    guest: Component,
    received: Received,
}

impl Forwarder {
    fn new() -> Self {
        let registry = carrier_registry();
        let (recipient, received) = recipient_sink(&registry);
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let ctx = ctx_at(registry, mailer, HubOutbound::disconnected(), MailboxId(0), None);
        Self { guest: instantiate_with_ctx(&forwarding_guest(recipient), ctx), received }
    }

    /// Deliver `payload`, attaching `attached`, and return the status the
    /// guest's forward got back.
    fn forward(&mut self, payload: Vec<u8>, attached: &[&Arc<BlobEntry>]) -> u32 {
        let attachments = attached.iter().map(|entry| Arc::clone(entry)).collect::<Vec<_>>();
        let mail = inbound(MailboxId(0), KindId(0), payload, Source::NONE)
            .with_attachments(Some(attachments.into_boxed_slice()));
        self.guest.deliver(&mail).expect("deliver");
        self.guest.read_u32(STATUS_AT)
    }

    fn received(&self) -> Vec<Envelope> {
        mem::take(&mut *self.received.lock().expect("sink lock"))
    }
}

/// Every byte of `blob`.
fn read_all(blob: &Blob) -> Vec<u8> {
    let reader = BlobReader::open(blob);
    let mut bytes = vec![0; usize::try_from(reader.len()).expect("fits")];
    let mut filled = 0;
    while filled < bytes.len() {
        let copied = reader.read_range(filled as u64, &mut bytes[filled..]);
        assert_ne!(copied, 0, "a blob reads to its length");
        filled += copied;
    }
    bytes
}

/// (d) A guest forwarding a tag-1 payload that names an entry its table pins
/// gets it through with that entry attached, so the recipient shares the
/// bytes. Catches a guest send whose hashes reach the recipient with nothing
/// behind them.
#[test]
fn a_guest_send_naming_a_pinned_entry_attaches_it() {
    let store = store();
    let entry = store.check_in(patterned(24));
    let mut forwarder = Forwarder::new();

    assert_eq!(forwarder.forward(tagged(&entry), &[&entry]), 0, "the send goes through");

    let received = forwarder.received();
    assert_eq!(received.len(), 1, "the recipient receives the forward once");
    assert_eq!(received[0].attachments().len(), 1);
    assert!(Arc::ptr_eq(&received[0].attachments()[0], &entry), "the forward attaches the pinned entry");
}

/// (e) A guest forwarding a hash its table neither pins nor holds is refused
/// with `SEND_BLOB_REFUSED` while its table holds other entries, and nothing
/// is routed. Catches a guessed or foreign hash being resolved, or leaving
/// the sender with nothing behind it.
#[test]
fn a_guest_send_naming_an_unheld_hash_is_refused() {
    let store = store();
    let pinned = store.check_in(patterned(24));
    let unheld = store.check_in(b"resident, but not this guest's".as_slice().into());
    let mut forwarder = Forwarder::new();

    assert_eq!(forwarder.forward(tagged(&unheld), &[&pinned]), SEND_BLOB_REFUSED);

    assert!(forwarder.received().is_empty(), "a refused send routes nothing");
}

/// (g) A guest whose table is empty forwards a tag-1 payload unwalked and
/// unattached, as ADR-0238 decision 3 says for senders that hold no blob; the
/// recipient's decode is what refuses it. Catches resolve on send running,
/// and refusing, for a blob-free guest.
#[test]
fn a_guest_with_an_empty_table_sends_unwalked() {
    let store = store();
    let entry = store.check_in(patterned(24));
    let mut forwarder = Forwarder::new();

    assert_eq!(forwarder.forward(tagged(&entry), &[]), 0, "the send goes through unwalked");

    let received = forwarder.received();
    assert_eq!(received.len(), 1);
    assert!(received[0].attachments().is_empty(), "nothing was resolved, so nothing is attached");
}

/// (f) A guest replying to a session with a payload naming an entry it holds
/// reaches the hub as tag 0 carrying the entry's bytes, which the plain
/// decoder reads. Catches a guest reply that leaves the process with a hash
/// no far decoder can resolve.
#[test]
fn a_guest_reply_to_a_session_leaves_as_bytes() {
    let store = store();
    let entry = store.check_in(patterned(40));
    let (outbound, egress) = HubOutbound::attached_loopback();
    let registry = carrier_registry();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let mut guest = instantiate_with_ctx(&replying_guest(), ctx_at(registry, mailer, outbound, MailboxId(0), None));
    let session = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0x6756))));

    guest
        .deliver(
            &inbound(MailboxId(0), KindId(0), tagged(&entry), session)
                .with_attachments(Some(Box::new([entry.clone()]))),
        )
        .expect("deliver");

    assert_eq!(guest.read_u32(STATUS_AT), REPLY_OK);
    let payload = egress
        .try_iter()
        .find_map(|event| match event {
            EgressEvent::ToSession { payload, .. } => Some(payload),
            _ => None,
        })
        .expect("the reply reaches the hub");
    let reply = GuestCarrier::decode_from_bytes(&payload).expect("the plain decoder reads a tag-0 reply");
    assert_eq!(read_all(&reply.blob), entry.bytes());
}

/// The payload of the asset section [`asset_guest`] carries.
const ASSET: &[u8] = b"tile-pixels-inside-the-module-file";

/// Where [`asset_guest`] keeps the name it asks its load window for.
const NAME_AT: usize = 600;

/// Where [`asset_guest`] builds its [`GuestCarrier`]: a tag-1 byte, then the
/// hash `asset_blob_p32` writes.
const CARRIER_AT: usize = 640;

/// Where [`asset_guest`] stores `asset_blob_p32`'s result.
const ASSET_LEN_AT: usize = 700;

/// A guest carrying [`ASSET`] in an `aether.asset.tile` section. On receive
/// it takes the asset named `name` through `asset_blob_p32`, with the hash
/// landing one byte into a tag-1 [`GuestCarrier`], stores the result at
/// [`ASSET_LEN_AT`], and forwards the carrier to `recipient`, storing the
/// status at [`STATUS_AT`]: what the SDK does for a guest that sends an
/// asset blob on as a `Blob` field.
fn asset_guest(recipient: MailboxId, name: &str) -> String {
    format!(
        r#"
        (module
            (@custom "aether.asset.tile" "{asset}")
            (import "aether" "asset_blob_p32" (func $asset_blob (param i32 i32 i32) (result i64)))
            (import "aether" "send_mail_p32"
                (func $send (param i64 i64 i32 i32 i32 i32 i64) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const {NAME_AT}) "{name}")
            (data (i32.const {CARRIER_AT}) "\01")
            (data (i32.const {STATUS_AT}) "\ff\ff\ff\ff")
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                (i64.store (i32.const {ASSET_LEN_AT}) (call $asset_blob
                    (i32.const {NAME_AT})
                    (i32.const {name_len})
                    (i32.const {hash_at})))
                (i32.store (i32.const {STATUS_AT}) (call $send
                    (i64.const {recipient})
                    (i64.const {carrier})
                    (i32.const {CARRIER_AT})
                    (i32.const 33)
                    (i32.const 1)
                    (i32.const 0)
                    (i64.const 0)))
                i32.const 0))
        "#,
        asset = str::from_utf8(ASSET).expect("the asset is text"),
        name_len = name.len(),
        hash_at = CARRIER_AT + 1,
        recipient = recipient.0,
        carrier = GuestCarrier::ID.0,
    )
}

/// An instantiated [`asset_guest`] with its load window open over its own
/// module code, as the trampoline leaves a loaded guest during `wire`.
struct AssetGuest {
    guest: Component,
    received: Received,
    /// The module file's code, the value the load brought.
    code: Blob,
    store: BlobStore,
}

impl AssetGuest {
    fn new(name: &str) -> Self {
        let registry = carrier_registry();
        let (recipient, received) = recipient_sink(&registry);
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let store = mailer.blob_store().clone();
        let blobs = BlobCheckIn::new(store.clone());
        let wat = asset_guest(recipient, name);

        let code = blobs.check_in(wat::parse_str(&wat).expect("compile WAT").into_boxed_slice());
        let module =
            ModuleCache::new(Arc::new(Engine::default())).check_in(&blobs, &code).expect("check the module in");
        let mut ctx = ctx_at(registry, mailer, HubOutbound::disconnected(), MailboxId(0), None);
        ctx.install_load_window(LoadWindow::open(&module, Some(code.clone())));

        Self { guest: instantiate_with_ctx(&wat, ctx), received, code, store }
    }

    /// Run the guest's receive: one `asset_blob_p32` call, then the forward.
    fn run(&mut self) -> wasmtime::Result<u32> {
        self.guest.deliver(&inbound(MailboxId(0), KindId(0), Vec::new(), Source::NONE))
    }

    fn received(&self) -> Vec<Envelope> {
        mem::take(&mut *self.received.lock().expect("sink lock"))
    }
}

/// A guest that takes an asset as a blob and sends it on attaches the view
/// entry itself: the recipient's bytes are the asset's, they lie inside the
/// module code's own buffer, and the store's resident bytes do not grow. The
/// hold the hostcall took then keeps the code resident on its own until the
/// instance goes. Catches a hostcall that copies the range into a new entry,
/// a hash written that the table does not hold (the send refused), and a
/// hold the table never releases.
#[test]
fn a_guest_forwarding_an_asset_blob_attaches_a_view_of_the_module_code() {
    let mut rig = AssetGuest::new("tile");
    let code_len = rig.store.resident_bytes();

    rig.run().expect("deliver");

    assert_eq!(stored(&mut rig.guest, ASSET_LEN_AT), i64::try_from(ASSET.len()).expect("fits"));
    assert_eq!(rig.guest.read_u32(STATUS_AT), 0, "the forward goes through");
    let received = rig.received();
    let [attached] = received[0].attachments() else {
        panic!("the forward attaches exactly the asset, got {}", received[0].attachments().len());
    };
    let code_bytes = rig.code.contiguous().expect("a store entry is contiguous").as_ptr_range();
    assert_eq!(attached.bytes(), ASSET);
    assert!(code_bytes.contains(&attached.bytes().as_ptr()), "the recipient reads the module code in place");
    assert_eq!(rig.store.resident_bytes(), code_len, "the asset was never copied into the store");

    let AssetGuest { guest, code, store, .. } = rig;
    drop(received);
    drop(code);

    assert_eq!(store.resident_bytes(), code_len, "the guest's hold alone keeps the module code resident");

    drop(guest);

    assert_eq!(store.resident_bytes(), 0, "the code leaves with the instance that held a view of it");
}

/// A name the module carries no asset under answers the sentinel and holds
/// nothing, so the carrier the guest forwards names nothing the host
/// attaches. Catches a missing asset answered as an empty blob, or one that
/// leaves an entry in the table.
#[test]
fn asset_blob_answers_a_sentinel_for_a_name_the_module_does_not_carry() {
    let mut rig = AssetGuest::new("none");

    rig.run().expect("deliver");

    assert_eq!(stored(&mut rig.guest, ASSET_LEN_AT), ASSET_BLOB_NOT_FOUND);
    assert!(rig.received()[0].attachments().is_empty(), "nothing was held, so nothing is attached");
}

/// Once the window has closed, as it has when `wire` returns, the hostcall
/// traps instead of serving: no payload path to the module file outlives the
/// window. Catches a handler-time `asset_blob_p32` that still reaches the
/// module's bytes, or reads as a missing asset.
#[test]
fn asset_blob_traps_once_the_load_window_has_closed() {
    let mut rig = AssetGuest::new("tile");
    rig.guest.close_load_window();

    let error = rig.run().expect_err("a call outside the window traps");

    assert!(format!("{error:?}").contains("outside the load window"), "error was: {error:?}");
    assert!(rig.received().is_empty(), "the trapped receive forwarded nothing");
}
