use std::fmt::Write as _;
use std::sync::Arc;

use aether_data::{Blob, CONTENT_ADDRESSED_SECTION, INPUTS_SECTION, INPUTS_SECTION_VERSION, InputsRecord, wire};
use wasmtime::Engine;

use super::{Module, ModuleCache};
use crate::actor::native::BlobCheckIn;
use crate::store::BlobStore;

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

/// `records` as an `aether.kinds.inputs` section payload, escaped for a WAT
/// `@custom` string.
fn escaped_inputs_section(records: &[InputsRecord]) -> String {
    let section = records.iter().fold(Vec::new(), |mut section, record| {
        section.push(INPUTS_SECTION_VERSION);
        section.extend(wire::to_vec(record).expect("encode an inputs record"));
        section
    });
    section.iter().fold(String::new(), |mut escaped, byte| {
        write!(escaped, "\\{byte:02x}").expect("write to a String");
        escaped
    })
}

fn same_entry(left: &Module, right: &Module) -> bool {
    Arc::ptr_eq(&left.entry, &right.entry)
}

fn same_compile(left: &Module, right: &Module) -> bool {
    Arc::ptr_eq(&left.entry.code, &right.entry.code)
}

/// A bundle over one fixed code carrying `payload` as its `sprite` asset.
fn bundle(payload: &str) -> String {
    format!(r#"(module (@custom "aether.asset.sprite" "{payload}") (func (export "noop")))"#)
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

/// The module holds neither its code nor any asset's payload: once the
/// caller drops the code blob, nothing of the module is resident in the
/// store, while the manifest still catalogues the asset. It catches a module
/// entry that keeps a bundle's payload (or its wasm bytes) resident for as
/// long as any instance or publication holds the module.
#[test]
fn a_module_holds_neither_its_code_nor_its_asset_payloads() {
    let store = store();
    let (cache, blobs) = (cache(), BlobCheckIn::new(store.clone()));
    let code = blobs.check_in(wasm(
        r#"(module (@custom "aether.asset.sprites/slime.png" "slime-sprite-bytes") (func (export "noop")))"#,
    ));

    let module = cache.check_in(&blobs, &code).expect("check the module in");
    drop(code);

    assert_eq!(store.resident_bytes(), 0, "a held module keeps no bytes resident");
    let [asset] = module.manifest().asset_catalog() else {
        panic!("one asset section is one catalog entry");
    };
    assert_eq!(asset.name, "sprites/slime.png");
    assert_eq!(asset.len, b"slime-sprite-bytes".len() as u64);
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

/// A content-addressed module publishes each export as `<namespace>.<hash>`,
/// which must stay one 256-byte segment, so an exported namespace longer than
/// 191 bytes fails check-in, naming it. It catches a qualified namespace over
/// the segment limit reaching the publication table, where it could never be
/// spawned, and a limit off by one either way.
#[test]
fn a_content_addressed_module_refuses_a_namespace_too_long_to_carry_its_hash() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));
    let content_addressed_exporting = |namespace: &str| {
        let escaped = escaped_inputs_section(&[InputsRecord::ActorBoundary { namespace: namespace.to_owned().into() }]);
        let wat = format!(
            r#"(module (@custom "{INPUTS_SECTION}" "{escaped}") (@custom "{CONTENT_ADDRESSED_SECTION}" "\01") (func (export "noop")))"#
        );
        cache.check_in(&blobs, &blobs.check_in(wasm(&wat)))
    };

    let longest = "a".repeat(191);
    content_addressed_exporting(&longest).expect("a 191-byte namespace carries its hash in one segment");

    let too_long = "a".repeat(192);
    let error = content_addressed_exporting(&too_long).map(drop).expect_err("a 192-byte namespace cannot carry it");
    assert!(error.contains(&format!("`{too_long}`")), "the refusal names the namespace: {error}");
}

/// `ModuleManifest::instanced` answers from the exported group its namespace
/// names, and a single-actor module's implicit group answers to the module's
/// namespace. It catches a lookup that reads the wrong group (the entry, or
/// the one after), one that misses the implicit group, and one that answers
/// for a name the module does not export.
#[test]
fn the_manifest_answers_each_exported_namespaces_cardinality() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));
    let module_with = |records: &[InputsRecord], namespace_section: &str| {
        let wat = format!(
            r#"(module (@custom "{INPUTS_SECTION}" "{}") {namespace_section} (func (export "noop")))"#,
            escaped_inputs_section(records)
        );
        check_in(&cache, &blobs, &wat)
    };

    let grouped = module_with(
        &[
            InputsRecord::ActorBoundary { namespace: "m.root".into() },
            InputsRecord::ActorBoundary { namespace: "m.panel".into() },
            InputsRecord::Instanced,
            InputsRecord::ActorBoundary { namespace: "m.status".into() },
        ],
        "",
    );
    let manifest = grouped.manifest();
    assert_eq!(manifest.instanced("m.root"), Some(false));
    assert_eq!(manifest.instanced("m.panel"), Some(true));
    assert_eq!(manifest.instanced("m.status"), Some(false));
    assert_eq!(manifest.instanced("m.absent"), None);

    let single = module_with(&[InputsRecord::Instanced], r#"(@custom "aether.namespace" "m.single")"#);
    assert_eq!(single.manifest().instanced("m.single"), Some(true));
}

/// Two bundles packed from one build differ only in an asset section. They
/// must stay two modules, each with its own hash and its own catalog, over
/// one compile. It catches a compile key that still covers the assets (one
/// compile per bundle, the cost this sharing removes), and a key so wide
/// that the second bundle answers the first's entry and so the first's
/// catalog and identity.
#[test]
fn bundles_that_differ_only_in_assets_are_two_modules_over_one_compile() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));

    let slime = check_in(&cache, &blobs, &bundle("slime"));
    let dragon = check_in(&cache, &blobs, &bundle("a-much-longer-dragon"));

    assert!(same_compile(&slime, &dragon), "one code is one compile");
    assert_eq!(cache.compiled_len(), 1);
    assert!(!same_entry(&slime, &dragon), "two files are two modules");
    assert_ne!(slime.hash(), dragon.hash());
    assert_eq!(slime.manifest().asset_catalog()[0].len, 5);
    assert_eq!(dragon.manifest().asset_catalog()[0].len, 20);
}

/// A bundle and the same build with no asset at all share one compile too:
/// the assetless file's code hash is its file hash, and the bundle's is the
/// hash of those same bytes. It catches a stripped bundle hashed or compiled
/// from bytes other than the file an assetless build is.
#[test]
fn a_bundle_shares_the_compile_of_its_assetless_build() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));

    let bare = check_in(&cache, &blobs, r#"(module (func (export "noop")))"#);
    let bundled = check_in(&cache, &blobs, &bundle("slime"));

    assert!(same_compile(&bare, &bundled), "a bundle's code is the build it was packed from");
}

/// Modules whose code differs must not share a compile, whatever assets they
/// carry in common. It catches a compile key that covers too little, which
/// would instantiate one module's code under another module's name.
#[test]
fn modules_that_differ_in_code_do_not_share_a_compile() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));
    let with_sprite = |export: &str| {
        let wat = format!(r#"(module (@custom "aether.asset.sprite" "slime") (func (export "{export}")))"#);
        check_in(&cache, &blobs, &wat)
    };

    let alpha = with_sprite("alpha");
    let beta = with_sprite("beta");

    assert!(!same_compile(&alpha, &beta), "different code is different compiles");
    assert!(alpha.compiled().get_export("alpha").is_some(), "alpha runs its own code");
    assert!(beta.compiled().get_export("beta").is_some(), "beta runs its own code");
}

/// Compiled code lives exactly as long as a module over it does: one bundle
/// dropping leaves it for the other, the last one dropping frees it, and its
/// dead slot is pruned. It catches a strong reference in the compiled-code
/// map, which would keep every bundle's megabyte of code for the engine's
/// life, and a slot that outlives its code.
#[test]
fn compiled_code_is_freed_once_the_last_module_over_it_drops() {
    let (cache, blobs) = (cache(), BlobCheckIn::new(store()));
    let slime = check_in(&cache, &blobs, &bundle("slime"));
    let dragon = check_in(&cache, &blobs, &bundle("dragon"));
    let weak_code = Arc::downgrade(&slime.entry.code);

    drop(slime);
    assert!(weak_code.upgrade().is_some(), "the remaining bundle still holds the shared code");

    drop(dragon);
    assert!(weak_code.upgrade().is_none(), "the map must hold no strong reference of its own");

    let _alpha = check_in(&cache, &blobs, ALPHA);
    assert_eq!(cache.compiled_len(), 1, "the freed code's slot is pruned rather than left to accumulate");
}
