//! Issue 7629: a guest pages a module in from the `objects` file namespace.
//!
//! The `test.object.loader` fixture reads a package object by its lowercase
//! hex sha256, publishes the blob the read answers with, and spawns a type
//! from it, answering its requester with the spawn's `SpawnResult`. The
//! harness roots `objects` at the test sandbox, where the object is a file
//! named by its hash, as it is under a package's `pack/objects`.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fmt::Write as _;
use std::fs;

use aether_harness_substrate::test_helpers::{init_save_sandbox, require_wasm, test_namespace_roots, write_fixture};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, SpawnResult};
use aether_test_fixtures_kinds::ObjectSpawn;
use sha2::{Digest, Sha256};

const SUBJECT: &str = "test.republish.subject";

/// The loader's row this file sends: `ObjectSpawn -> SpawnResult`. The loader
/// ships only as a cdylib example, so the test casts its `load_any` reference
/// to this instead of naming a type.
#[aether_actor::protocol]
trait ObjectLoaderRow {
    fn spawn(mail: ObjectSpawn) -> SpawnResult;
}

/// The name an object takes in the store: the lowercase hex sha256 of its bytes.
fn object_name(bytes: &[u8]) -> String {
    let mut name = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(name, "{byte:02x}").expect("writing to a String never fails");
    }
    name
}

/// The bug this catches: the `objects` namespace is not registered or not
/// rooted where the chassis was told, or a blob a guest forwards from a read
/// into `Publish.code` does not reach the component host as the module's
/// code. Either leaves the loader answering `SpawnResult::Err`.
#[test]
fn a_guest_reads_an_object_by_hash_then_publishes_and_spawns_it() {
    let Some(loader_path) = require_wasm("object_loader") else {
        return;
    };
    let Some(subject_path) = require_wasm("republish_subject_base") else {
        return;
    };
    let sandbox = init_save_sandbox("object-load");
    let mut harness = SubstrateHarness::builder()
        .with_component_host()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    let subject_wasm = fs::read(subject_path).expect("read subject wasm");
    let hash = write_fixture(&object_name(&subject_wasm), &subject_wasm);
    let loader_wasm = fs::read(loader_path).expect("read loader wasm");
    let (loader, _) = harness
        .load_any(&LoadComponent { wasm: loader_wasm, name: None, config: Vec::new(), export: None })
        .expect("load the object loader");
    let loader = harness.cast::<ObjectLoaderRow>(loader).expect("the loader publishes ObjectSpawn");

    let spawned = harness
        .execute(vec![(
            "spawn",
            HarnessOp::send_and_await_reply(&loader, &ObjectSpawn { hash, namespace: SUBJECT.to_owned() }),
        )])
        .expect("ObjectSpawn to the loader")
        .reply::<SpawnResult>("spawn")
        .expect("decode the loader's reply");

    match spawned {
        SpawnResult::Spawned { path, .. } => assert_eq!(path.as_str(), SUBJECT, "the subject is a root singleton"),
        other => panic!("the object was not read, published and spawned: {other:?}"),
    }
    let names = harness.list_components().expect("list components");
    assert!(names.iter().any(|name| name == SUBJECT), "the spawned subject is live: {names:?}");
}
