//! The guest blob table and host fns (`blob_hold_p32`, `blob_read_p32`,
//! `blob_drop_p32`, ADR-0238 decisions 2, 3, 4 and 9). Delivery tests drive
//! `Component::deliver` with an attached mail into a WAT guest whose
//! `receive_p32` acts on the hash at the start of its payload; the host fn
//! tests drive a WAT guest whose exports forward straight to the imports,
//! over entries pinned through `BlobTable::pin`. Entries come from a fresh
//! `BlobStore`.

use std::sync::Arc;

use aether_data::{KindId, MAX_READ_BYTES};
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store};

use super::{
    Component, DISPATCH_DROPPED_OVERSIZE, MAX_DELIVERABLE_MAIL_BYTES, WAT_HOOKS, WAT_REALLOC, ctx, inbound, instantiate,
};
use crate::actor::native::envelope::Envelope;
use crate::actor::wasm::ComponentCtx;
use crate::actor::wasm::host_fns::{self, BLOB_NOT_HELD, BLOB_OUT_OF_BOUNDS};
use crate::mail::{MailboxId, Source};
use crate::store::{BlobEntry, BlobStore};

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
