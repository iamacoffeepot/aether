use super::{
    ArtifactKind, ArtifactStore, DEFAULT_DISK_BUDGET_BYTES, ListComponentBinaries,
    ListEngineBinaries, Selector, StoredManifest,
};
use aether_data::Kind;
use aether_kinds::{ComponentActor, ComponentManifest, Key, Tick};
use std::path::PathBuf;
use std::{env, fs, process};

fn temp_root(label: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "aether-binstore-test-{label}-{}-{}",
        process::id(),
        super::persistence::now_nanos()
    ))
}

fn manifest(chassis: &str) -> StoredManifest {
    StoredManifest::Binary(aether_kinds::BinaryManifest {
        chassis: chassis.to_owned(),
        caps: vec!["aether.fs".to_owned()],
        git_sha: "deadbee".to_owned(),
        profile: "debug".to_owned(),
        target: "x86_64-unknown-linux-gnu".to_owned(),
    })
}

/// A component manifest exporting `namespace`, handling `Tick` + `Key`,
/// for the ADR-0116 component-store resolve/list unit tests.
fn component_manifest(namespace: &str) -> StoredManifest {
    StoredManifest::Component(ComponentManifest {
        namespaces: vec![namespace.to_owned()],
        actors: vec![ComponentActor {
            namespace: namespace.to_owned(),
            handled_kinds: vec![Tick::ID, Key::ID],
            fallback: false,
        }],
        handled_kinds: vec![Tick::ID, Key::ID],
        fallback: false,
        provenance: String::new(),
    })
}

#[test]
fn upload_dedups_identical_bytes_to_one_hash() {
    let root = temp_root("dedup");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
    let h1 = store.upload(
        b"the-binary-bytes",
        ArtifactKind::Binary,
        manifest("headless"),
        None,
    );
    let h2 = store.upload(
        b"the-binary-bytes",
        ArtifactKind::Binary,
        manifest("headless"),
        None,
    );
    assert_eq!(h1, h2, "identical bytes dedup to the same content hash");
    assert_eq!(store.entry_count(), 1, "dedup stores one entry");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn component_store_uploads_dedups_and_resolves_by_attribute() {
    // ADR-0116, issue 1956: the artifact-generic store holds component
    // wasm as a second type-tagged artifact. Upload + dedup, name
    // repoint, resolve by hash / name, list by namespace / handled-kind.
    let root = temp_root("component");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);

    let h1 = store.upload(
        b"probe-wasm-bytes",
        ArtifactKind::Component,
        component_manifest("test_fixture_probe"),
        Some("probe".to_owned()),
    );
    // An identical re-upload dedups to the same hash.
    let h2 = store.upload(
        b"probe-wasm-bytes",
        ArtifactKind::Component,
        component_manifest("test_fixture_probe"),
        None,
    );
    assert_eq!(h1, h2, "identical component bytes dedup to one hash");
    assert_eq!(store.entry_count(), 1, "dedup stores one component entry");

    // Resolve by hash and by name; the manifest is the component one.
    let by_hash = store
        .get(&Selector::Hash(h1.clone()))
        .expect("the component resolves by hash");
    assert_eq!(by_hash.kind, ArtifactKind::Component);
    assert!(
        by_hash.manifest.as_component().is_some(),
        "a component entry carries a component manifest",
    );
    let by_name = store
        .get(&Selector::Name("probe".to_owned()))
        .expect("the component resolves by name");
    assert_eq!(by_name.hash, h1, "the name points at the component hash");

    // A component is not listed as a binary, and vice versa.
    store.upload(
        b"a-binary",
        ArtifactKind::Binary,
        manifest("headless"),
        None,
    );
    assert_eq!(
        store.list_binaries(&ListEngineBinaries::default()).len(),
        1,
        "only the binary lists as a binary",
    );
    let components = store.list_components(&ListComponentBinaries::default());
    assert_eq!(
        components.len(),
        1,
        "only the component lists as a component"
    );
    assert_eq!(components[0].hash, h1);

    // Attribute filters: namespace + handled-kind keep the entry; a
    // miss drops it.
    let by_namespace = store.list_components(&ListComponentBinaries {
        namespace: Some("test_fixture_probe".to_owned()),
        handled_kind: None,
    });
    assert_eq!(by_namespace.len(), 1, "a matching namespace keeps it");
    let by_kind = store.list_components(&ListComponentBinaries {
        namespace: None,
        handled_kind: Some(Tick::ID),
    });
    assert_eq!(by_kind.len(), 1, "a matching handled-kind keeps it");
    let miss = store.list_components(&ListComponentBinaries {
        namespace: Some("nope".to_owned()),
        handled_kind: None,
    });
    assert!(miss.is_empty(), "a non-matching namespace drops it");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn name_repoints_to_the_latest_uploaded_hash() {
    let root = temp_root("repoint");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
    let h_old = store.upload(
        b"v1",
        ArtifactKind::Binary,
        manifest("headless"),
        Some("engine".to_owned()),
    );
    let h_new = store.upload(
        b"v2",
        ArtifactKind::Binary,
        manifest("headless"),
        Some("engine".to_owned()),
    );
    assert_ne!(h_old, h_new);
    let resolved = store
        .get(&Selector::Name("engine".to_owned()))
        .expect("the name resolves");
    assert_eq!(resolved.hash, h_new, "the name points at the latest upload");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn entries_persist_across_a_reopen() {
    let root = temp_root("persist");
    let hash = {
        let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
        store.upload(
            b"persisted-bytes",
            ArtifactKind::Binary,
            manifest("headless"),
            Some("svc".to_owned()),
        )
        // store drops here — LockGuard releases lock.pid
    };
    let mut reopened = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
    assert!(reopened.contains(&hash), "the entry survives a reopen");
    let resolved = reopened
        .get(&Selector::Name("svc".to_owned()))
        .expect("the name survives a reopen");
    assert_eq!(resolved.hash, hash);
    let bytes = fs::read(&resolved.path).expect("the stored bytes are readable");
    assert_eq!(bytes, b"persisted-bytes");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn eviction_skips_pinned_and_named_entries() {
    let root = temp_root("evict");
    // Budget holds the three ~10-byte initial entries (≈31 bytes) but
    // not a fourth, so the trigger upload forces exactly one eviction —
    // of the only unnamed, unpinned candidate.
    let mut store = ArtifactStore::open(&root, 40);
    // Unnamed, unpinned — the eviction candidate.
    let h_plain = store.upload(
        b"plain-aaaa",
        ArtifactKind::Binary,
        manifest("headless"),
        None,
    );
    // Named — protected.
    let h_named = store.upload(
        b"named-bbbb",
        ArtifactKind::Binary,
        manifest("headless"),
        Some("keep".to_owned()),
    );
    // Pinned — protected.
    let h_pinned = store.upload(
        b"pinned-cccc",
        ArtifactKind::Binary,
        manifest("headless"),
        None,
    );
    assert!(store.pin(&h_pinned), "pin targets a stored entry");
    // Force eviction by re-running it through another over-budget upload.
    let _ = store.upload(
        b"trigger-dddd",
        ArtifactKind::Binary,
        manifest("headless"),
        None,
    );

    assert!(store.contains(&h_named), "a named entry is never evicted");
    assert!(store.contains(&h_pinned), "a pinned entry is never evicted");
    assert!(
        !store.contains(&h_plain),
        "the oldest unnamed, unpinned entry is evicted first",
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn list_applies_chassis_and_caps_filters() {
    let root = temp_root("filter");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
    store.upload(
        b"headless-bin",
        ArtifactKind::Binary,
        manifest("headless"),
        None,
    );
    let desktop = match manifest("desktop") {
        StoredManifest::Binary(mut m) => {
            m.caps = vec!["aether.fs".to_owned(), "aether.render".to_owned()];
            StoredManifest::Binary(m)
        }
        StoredManifest::Component(_) => unreachable!("manifest() returns a binary manifest"),
    };
    store.upload(b"desktop-bin", ArtifactKind::Binary, desktop, None);

    let headless_only = store.list_binaries(&ListEngineBinaries {
        chassis: Some("headless".to_owned()),
        caps: vec![],
        target: None,
    });
    assert_eq!(headless_only.len(), 1);
    assert_eq!(headless_only[0].manifest.chassis, "headless");

    let render_capable = store.list_binaries(&ListEngineBinaries {
        chassis: None,
        caps: vec!["aether.render".to_owned()],
        target: None,
    });
    assert_eq!(
        render_capable.len(),
        1,
        "only the desktop binary links render",
    );
    assert_eq!(render_capable[0].manifest.chassis, "desktop");

    let all = store.list_binaries(&ListEngineBinaries::default());
    assert_eq!(all.len(), 2);
    let _ = fs::remove_dir_all(&root);
}
