//! The guest blob host fns (`blob_len_p32`, `blob_read_p32`,
//! `blob_drop_p32`, ADR-0238 decisions 2, 4 and 9) against one instance's
//! blob table, driven from a WAT guest whose exports forward straight to the
//! imports. Entries come from a fresh `BlobStore` and reach the table through
//! the `BlobTable::grant` test seam that delivery will replace.

use std::sync::Arc;

use aether_data::MAX_READ_BYTES;
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store};

use super::{WAT_HOOKS, ctx, instantiate};
use crate::actor::wasm::ComponentCtx;
use crate::actor::wasm::host_fns::{self, BLOB_NOT_HELD, BLOB_OUT_OF_BOUNDS};
use crate::store::{BlobEntry, BlobStore};

/// A guest whose `len` / `read` / `drop` exports forward their arguments to
/// the blob imports. 40 pages of memory (2.5 MiB) leave room for a
/// destination past `MAX_READ_BYTES`.
const WAT_BLOB_GUEST: &str = r#"
        (module
            (import "aether" "blob_len_p32" (func $len (param i32) (result i64)))
            (import "aether" "blob_read_p32" (func $read (param i32 i64 i32 i32) (result i64)))
            (import "aether" "blob_drop_p32" (func $drop (param i32)))
            (memory (export "memory") 40)
            (func (export "len") (param i32) (result i64)
                local.get 0
                call $len)
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

    /// Add one count for `entry` to this instance's table.
    fn grant(&mut self, entry: &Arc<BlobEntry>) {
        self.store.data_mut().blob_table.grant(Arc::clone(entry));
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

    /// Call `blob_len_p32`. A trap fails the test.
    fn len(&mut self, hash_ptr: u32) -> i64 {
        let len = self.instance.get_typed_func::<u32, i64>(&mut self.store, "len").expect("len export");
        len.call(&mut self.store, hash_ptr).expect("blob_len_p32 does not trap")
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

fn store() -> BlobStore {
    BlobStore::new().expect("spawn the reclaim thread")
}

/// `len` bytes that differ at every offset a test reads from.
fn patterned(len: usize) -> Box<[u8]> {
    (0..=250).cycle().take(len).collect()
}

/// Catches a wrong slice: reading from the wrong offset, past the requested
/// length, or past the blob's end.
#[test]
fn len_and_read_at_an_offset_copy_the_right_bytes() {
    let store = store();
    let entry = store.check_in(patterned(64));
    let mut guest = Guest::new();
    guest.grant(&entry);
    let hash = guest.name(&entry);

    assert_eq!(guest.len(hash), 64);

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
    other.grant(&elsewhere);
    let mut guest = Guest::new();
    guest.grant(&held);

    let hash = guest.name(&elsewhere);

    assert_eq!(guest.len(hash), BLOB_NOT_HELD);
    assert_eq!(guest.read(hash, 0, DST_AT, 8), BLOB_NOT_HELD);
    assert_eq!(guest.bytes(DST_AT, 8), [0; 8], "a refused read writes nothing");

    guest.drop_hold(hash);
    let hash = other.name(&elsewhere);
    assert_eq!(other.len(hash), 24, "a refused drop leaves the holder's count alone");
}

/// Catches a count that never reaches zero, one that reaches it early, and a
/// table that leaks the entry's `Arc` after its last count.
#[test]
fn the_last_drop_of_two_grants_frees_the_entry() {
    let store = store();
    let entry = store.check_in(patterned(32));
    let mut guest = Guest::new();
    guest.grant(&entry);
    guest.grant(&entry);
    let hash = guest.name(&entry);
    drop(entry);

    guest.drop_hold(hash);

    assert_eq!(guest.len(hash), 32, "one count remains");
    assert_eq!(store.resident_bytes(), 32);

    guest.drop_hold(hash);

    assert_eq!(store.resident_bytes(), 0);
    assert_eq!(guest.read(hash, 0, DST_AT, 8), BLOB_NOT_HELD);
}

/// Catches an unchecked copy: a hash or destination range that runs past the
/// end of guest memory must be refused with nothing written.
#[test]
fn out_of_bounds_pointers_are_refused_and_write_nothing() {
    let store = store();
    let entry = store.check_in(patterned(16));
    let mut guest = Guest::new();
    guest.grant(&entry);
    let hash = guest.name(&entry);
    let end = guest.memory_len();

    assert_eq!(guest.len(end - 16), BLOB_OUT_OF_BOUNDS, "a hash straddling the end of memory");
    assert_eq!(guest.read(end - 16, 0, DST_AT, 8), BLOB_OUT_OF_BOUNDS);

    assert_eq!(guest.read(hash, 0, end - 4, 8), BLOB_OUT_OF_BOUNDS, "a destination straddling the end");
    assert_eq!(guest.bytes(end - 4, 4), [0; 4]);

    guest.drop_hold(end - 16);
    assert_eq!(guest.len(hash), 16, "an out-of-bounds drop releases nothing");
}

/// Catches a missing host clamp: a destination larger than `MAX_READ_BYTES`
/// receives exactly `MAX_READ_BYTES`, and nothing past them.
#[test]
fn a_read_copies_at_most_max_read_bytes() {
    let store = store();
    let entry = store.check_in(patterned(MAX_READ_BYTES + 100));
    let mut guest = Guest::new();
    guest.grant(&entry);
    let hash = guest.name(&entry);
    let dst = 4096;
    let window = u32::try_from(MAX_READ_BYTES + 100).expect("the window fits the 32-bit ABI");

    assert_eq!(guest.read(hash, 0, dst, window), i64::try_from(MAX_READ_BYTES).expect("fits"));
    assert_eq!(guest.bytes(dst, MAX_READ_BYTES), entry.bytes()[..MAX_READ_BYTES]);
    assert_eq!(guest.bytes(dst + u32::try_from(MAX_READ_BYTES).expect("fits"), 100), [0; 100]);
}

/// Catches a teardown that keeps entries: dropping a `Component` whose table
/// still counts an entry must release it.
#[test]
fn dropping_a_component_releases_what_its_table_still_counts() {
    let store = store();
    let entry = store.check_in(patterned(48));
    let mut component = instantiate(WAT_HOOKS);
    component.store.data_mut().blob_table.grant(Arc::clone(&entry));
    component.store.data_mut().blob_table.grant(Arc::clone(&entry));
    drop(entry);

    assert_eq!(store.resident_bytes(), 48);

    drop(component);

    assert_eq!(store.resident_bytes(), 0);
}
