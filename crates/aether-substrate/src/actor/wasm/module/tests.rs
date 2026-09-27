use std::sync::Arc;

use aether_data::Blob;
use wasmtime::Engine;

use super::{Module, ModuleCache};
use crate::actor::native::BlobCheckIn;
use crate::store::{BlobStore, read_all};

const ALPHA: &str = r#"(module (func (export "alpha")))"#;
const BETA: &str = r#"(module (func (export "beta")))"#;

fn store() -> BlobStore {
    BlobStore::new().expect("spawn the reclaim thread")
}

fn cache() -> ModuleCache {
    ModuleCache::new(Arc::new(Engine::default()))
}

fn wasm(wat: &str) -> Box<[u8]> {
    wat::parse_str(wat).expect("parse the fixture WAT").into_boxed_slice()
}

/// Check `wat` in as code and build its module through `cache`.
fn check_in(cache: &ModuleCache, blobs: &BlobCheckIn, wat: &str) -> Module {
    cache.check_in(blobs, &blobs.check_in(wasm(wat))).expect("check the module in")
}

fn same_entry(left: &Module, right: &Module) -> bool {
    Arc::ptr_eq(&left.entry, &right.entry)
}

/// Every hash currently held live must answer with its own module, even when
/// another hash's check-in interleaves. A key that collapsed two artifacts
/// together would hand a load the wrong module's code and instantiate the
/// wrong component under the requested name, with no error anywhere on the
/// load path. A regression to one-slot behaviour, where holding `alpha` while
/// `beta` compiles evicts `alpha` and forces a recompile, breaks the entry
/// identity checks below rather than merely costing time.
#[test]
fn every_live_module_is_answered_for_its_own_content_hash() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));

    let alpha = check_in(&cache, &blobs, ALPHA);
    let beta = check_in(&cache, &blobs, BETA);

    let alpha_again = check_in(&cache, &blobs, ALPHA);
    let beta_again = check_in(&cache, &blobs, BETA);

    assert!(same_entry(&alpha, &alpha_again), "alpha's hash must answer with alpha's own module");
    assert!(same_entry(&beta, &beta_again), "beta's hash must answer with beta's own module");
    assert!(alpha.compiled().get_export("alpha").is_some(), "alpha's hash must expose alpha's own export");
    assert!(beta.compiled().get_export("beta").is_some(), "beta's hash must expose beta's own export");
}

/// A hash whose last holder drops must leave the map: never evicted by count
/// or capacity (ADR-0240 D5), but never left to accumulate once nothing
/// references it either. A later check-in of that hash must compile again
/// rather than upgrade a dead weak reference or fail.
#[test]
fn a_module_leaves_the_map_once_its_last_holder_drops() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));

    let alpha = check_in(&cache, &blobs, ALPHA);
    let weak_alpha = Arc::downgrade(&alpha.entry);
    drop(alpha);
    assert!(weak_alpha.upgrade().is_none(), "the map must hold no strong reference of its own");

    let _beta = check_in(&cache, &blobs, BETA);
    assert_eq!(cache.len(), 1, "the freed alpha entry is pruned rather than left to accumulate");

    let alpha_again = check_in(&cache, &blobs, ALPHA);
    assert!(alpha_again.compiled().get_export("alpha").is_some(), "a dead entry compiles again rather than failing");
}

/// The module must not hold its code: once the caller drops the code blob,
/// the store holds only the asset slab, and the asset blob reads back exactly
/// its section's payload. It catches a module that retains the wasm bytes, or
/// an asset served from the wrong range.
#[test]
fn the_code_bytes_leave_the_store_once_the_code_blob_drops() {
    let store = store();
    let (cache, blobs) = (cache(), BlobCheckIn::new(store.clone()));
    let payload: &[u8] = b"slime-sprite-bytes";
    let code = blobs.check_in(wasm(
        r#"(module (@custom "aether.asset.sprites/slime.png" "slime-sprite-bytes") (func (export "noop")))"#,
    ));

    let module = cache.check_in(&blobs, &code).expect("check the module in");
    drop(code);

    assert_eq!(store.resident_bytes(), payload.len(), "only the asset slab stays resident");
    let [(name, asset)] = module.assets() else {
        panic!("one asset section is one asset blob, got {:?}", module.assets());
    };
    assert_eq!(name.as_str(), "sprites/slime.png");
    assert_eq!(&*read_all(asset).expect("read the asset blob"), payload);
}

/// `Owned` code bytes and the same bytes already checked in must answer one
/// module. It catches an `Owned` path that keys by a different hash, or that
/// compiles a second time.
#[test]
fn an_owned_code_value_answers_the_module_its_checked_in_twin_does() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));

    let checked_in = check_in(&cache, &blobs, ALPHA);
    let owned = cache.check_in(&blobs, &Blob::from(wasm(ALPHA).into_vec())).expect("check owned code in");

    assert!(same_entry(&checked_in, &owned), "equal bytes answer one module whatever their backing");
    assert_eq!(checked_in.hash(), owned.hash());
}
