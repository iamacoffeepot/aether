use super::{
    ArtifactKind, ArtifactStore, DEFAULT_DISK_BUDGET_BYTES, ListComponentBinaries, ListEngineBinaries, Selector,
    StoredManifest,
};
use aether_data::Kind;
use aether_kinds::{ComponentActor, ComponentManifest, Key, Tick};
use std::path::PathBuf;
use std::{env, fs, process};

fn temp_root(label: &str) -> PathBuf {
    env::temp_dir().join(format!("aether-binstore-test-{label}-{}-{}", process::id(), super::persistence::now_nanos()))
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
        // A single-actor module has an unambiguous bare-load entry.
        default_entry: Some(namespace.to_owned()),
    })
}

#[test]
fn upload_dedups_identical_bytes_to_one_hash() {
    let root = temp_root("dedup");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let h1 = store.upload(b"the-binary-bytes", ArtifactKind::Binary, manifest("headless"), None);
    let h2 = store.upload(b"the-binary-bytes", ArtifactKind::Binary, manifest("headless"), None);
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
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");

    let h1 = store.upload(
        b"probe-wasm-bytes",
        ArtifactKind::Component,
        component_manifest("test_fixture_probe"),
        Some("probe".to_owned()),
    );
    // An identical re-upload dedups to the same hash.
    let h2 = store.upload(b"probe-wasm-bytes", ArtifactKind::Component, component_manifest("test_fixture_probe"), None);
    assert_eq!(h1, h2, "identical component bytes dedup to one hash");
    assert_eq!(store.entry_count(), 1, "dedup stores one component entry");

    // Resolve by hash and by name; the manifest is the component one.
    let by_hash = store.get(&Selector::Hash(h1.clone())).expect("the component resolves by hash");
    assert_eq!(by_hash.kind, ArtifactKind::Component);
    assert!(by_hash.manifest.as_component().is_some(), "a component entry carries a component manifest");
    let by_name = store.get(&Selector::Name("probe".to_owned())).expect("the component resolves by name");
    assert_eq!(by_name.hash, h1, "the name points at the component hash");

    // A component is not listed as a binary, and vice versa.
    store.upload(b"a-binary", ArtifactKind::Binary, manifest("headless"), None);
    assert_eq!(store.matching_binaries(&ListEngineBinaries::default()).len(), 1, "only the binary lists as a binary");
    let components = store.matching_components(&ListComponentBinaries::default());
    assert_eq!(components.len(), 1, "only the component lists as a component");
    assert_eq!(components[0].hash, h1);

    // Attribute filters: namespace + handled-kind keep the entry; a
    // miss drops it.
    let by_namespace = store.matching_components(&ListComponentBinaries {
        namespace: Some("test_fixture_probe".to_owned()),
        handled_kind: None,
        limit: None,
        include_history: false,
    });
    assert_eq!(by_namespace.len(), 1, "a matching namespace keeps it");
    let by_kind = store.matching_components(&ListComponentBinaries {
        namespace: None,
        handled_kind: Some(Tick::ID),
        limit: None,
        include_history: false,
    });
    assert_eq!(by_kind.len(), 1, "a matching handled-kind keeps it");
    let miss = store.matching_components(&ListComponentBinaries {
        namespace: Some("nope".to_owned()),
        handled_kind: None,
        limit: None,
        include_history: false,
    });
    assert!(miss.is_empty(), "a non-matching namespace drops it");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn name_repoints_to_the_latest_uploaded_hash() {
    let root = temp_root("repoint");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let h_old = store.upload(b"v1", ArtifactKind::Binary, manifest("headless"), Some("engine".to_owned()));
    let h_new = store.upload(b"v2", ArtifactKind::Binary, manifest("headless"), Some("engine".to_owned()));
    assert_ne!(h_old, h_new);
    let resolved = store.get(&Selector::Name("engine".to_owned())).expect("the name resolves");
    assert_eq!(resolved.hash, h_new, "the name points at the latest upload");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn entries_persist_across_a_reopen() {
    let root = temp_root("persist");
    let hash = {
        let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
        store.upload(b"persisted-bytes", ArtifactKind::Binary, manifest("headless"), Some("svc".to_owned()))
        // store drops here — LockGuard releases lock.pid
    };
    let mut reopened = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    assert!(reopened.contains(&hash), "the entry survives a reopen");
    let resolved = reopened.get(&Selector::Name("svc".to_owned())).expect("the name survives a reopen");
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
    let mut store = ArtifactStore::open(&root, 40).expect("open store");
    // Unnamed, unpinned — the eviction candidate.
    let h_plain = store.upload(b"plain-aaaa", ArtifactKind::Binary, manifest("headless"), None);
    // Named — protected.
    let h_named = store.upload(b"named-bbbb", ArtifactKind::Binary, manifest("headless"), Some("keep".to_owned()));
    // Pinned — protected.
    let h_pinned = store.upload(b"pinned-cccc", ArtifactKind::Binary, manifest("headless"), None);
    assert!(store.pin(&h_pinned), "pin targets a stored entry");
    // Force eviction by re-running it through another over-budget upload.
    let _ = store.upload(b"trigger-dddd", ArtifactKind::Binary, manifest("headless"), None);

    assert!(store.contains(&h_named), "a named entry is never evicted");
    assert!(store.contains(&h_pinned), "a pinned entry is never evicted");
    assert!(!store.contains(&h_plain), "the oldest unnamed, unpinned entry is evicted first");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn list_applies_chassis_and_caps_filters() {
    let root = temp_root("filter");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    store.upload(b"headless-bin", ArtifactKind::Binary, manifest("headless"), None);
    let desktop = match manifest("desktop") {
        StoredManifest::Binary(mut m) => {
            m.caps = vec!["aether.fs".to_owned(), "aether.render".to_owned()];
            StoredManifest::Binary(m)
        }
        StoredManifest::Component(_) => unreachable!("manifest() returns a binary manifest"),
    };
    store.upload(b"desktop-bin", ArtifactKind::Binary, desktop, None);

    let headless_only = store.matching_binaries(&ListEngineBinaries {
        chassis: Some("headless".to_owned()),
        caps: vec![],
        target: None,
        limit: None,
        include_history: false,
    });
    assert_eq!(headless_only.len(), 1);
    assert_eq!(headless_only[0].manifest.chassis, "headless");

    let render_capable = store.matching_binaries(&ListEngineBinaries {
        chassis: None,
        caps: vec!["aether.render".to_owned()],
        target: None,
        limit: None,
        include_history: false,
    });
    assert_eq!(render_capable.len(), 1, "only the desktop binary links render");
    assert_eq!(render_capable[0].manifest.chassis, "desktop");

    let all = store.matching_binaries(&ListEngineBinaries::default());
    assert_eq!(all.len(), 2);
    let _ = fs::remove_dir_all(&root);
}

fn binary_page_filter(limit: Option<u32>, include_history: bool) -> ListEngineBinaries {
    ListEngineBinaries { chassis: None, caps: Vec::new(), target: None, limit, include_history }
}

#[test]
fn listing_pages_apply_limits_after_counting_and_keep_raw_matches_uncapped() {
    let root = temp_root("listing-cap");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let mut hashes = Vec::new();
    for index in 0..25 {
        hashes.push(store.upload(
            format!("binary-{index}").as_bytes(),
            ArtifactKind::Binary,
            manifest("headless"),
            Some(format!("engine-{index}")),
        ));
    }

    let default_page = store.list_binaries_page(&binary_page_filter(None, false));
    assert_eq!(default_page.total_matched, 25, "the count is recorded before the default cap");
    assert_eq!(default_page.binaries.len(), 20, "the default page is capped at twenty entries");
    assert_eq!(
        default_page.binaries.iter().map(|entry| &entry.hash).collect::<Vec<_>>(),
        hashes.iter().rev().take(20).collect::<Vec<_>>(),
        "new hashes list newest-first by first ingest"
    );

    let zero_page = store.list_binaries_page(&binary_page_filter(Some(0), false));
    assert!(zero_page.binaries.is_empty(), "an explicit zero limit returns no entries");
    assert_eq!(zero_page.total_matched, 25, "zero limit preserves the pre-cap count");
    assert_eq!(store.list_binaries_page(&binary_page_filter(Some(3), false)).binaries.len(), 3);
    assert_eq!(store.list_binaries_page(&binary_page_filter(Some(30), false)).binaries.len(), 25);
    assert_eq!(
        store.matching_binaries(&ListEngineBinaries::default()).len(),
        25,
        "selector-facing raw matching remains uncapped"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn listing_pages_exclude_unnamed_history_unless_requested() {
    let root = temp_root("listing-history");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let named = store.upload(b"named", ArtifactKind::Binary, manifest("headless"), Some("live".to_owned()));
    let unnamed = store.upload(b"unnamed", ArtifactKind::Binary, manifest("headless"), None);

    let live = store.list_binaries_page(&binary_page_filter(None, false));
    assert_eq!(live.total_matched, 1);
    assert_eq!(live.binaries.iter().map(|entry| &entry.hash).collect::<Vec<_>>(), vec![&named]);

    let history = store.list_binaries_page(&binary_page_filter(None, true));
    assert_eq!(history.total_matched, 2);
    assert_eq!(
        history.binaries.iter().map(|entry| &entry.hash).collect::<Vec<_>>(),
        vec![&unnamed, &named],
        "history still uses stable newest-first ordering"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn component_listing_pages_apply_the_same_history_count_and_limit_contract() {
    let root = temp_root("component-listing-page");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let named = store.upload(
        b"named-component",
        ArtifactKind::Component,
        component_manifest("test.named"),
        Some("named".to_owned()),
    );
    let unnamed = store.upload(b"unnamed-component", ArtifactKind::Component, component_manifest("test.unnamed"), None);

    let live = store.list_components_page(&ListComponentBinaries::default());
    assert_eq!(live.total_matched, 1);
    assert_eq!(live.components[0].hash, named);
    let history = store.list_components_page(&ListComponentBinaries {
        limit: None,
        include_history: true,
        ..ListComponentBinaries::default()
    });
    assert_eq!(history.total_matched, 2);
    assert_eq!(history.components.iter().map(|entry| &entry.hash).collect::<Vec<_>>(), vec![&unnamed, &named]);
    let zero = store.list_components_page(&ListComponentBinaries {
        limit: Some(0),
        include_history: true,
        ..ListComponentBinaries::default()
    });
    assert!(zero.components.is_empty());
    assert_eq!(zero.total_matched, 2);
    assert_eq!(
        store.matching_components(&ListComponentBinaries::default()).len(),
        2,
        "selector-facing component matching remains uncapped and includes history"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn first_ingest_order_survives_dedup_and_reopen() {
    let root = temp_root("listing-persisted-order");
    let (older, newer) = {
        let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
        let older = store.upload(b"older", ArtifactKind::Binary, manifest("headless"), Some("older".to_owned()));
        let newer = store.upload(b"newer", ArtifactKind::Binary, manifest("headless"), Some("newer".to_owned()));
        let duplicate =
            store.upload(b"older", ArtifactKind::Binary, manifest("headless"), Some("older-again".to_owned()));
        assert_eq!(duplicate, older);
        let page = store.list_binaries_page(&binary_page_filter(None, false));
        assert_eq!(page.binaries.iter().map(|entry| &entry.hash).collect::<Vec<_>>(), vec![&newer, &older]);
        (older, newer)
    };

    let reopened = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let page = reopened.list_binaries_page(&binary_page_filter(None, false));
    assert_eq!(
        page.binaries.iter().map(|entry| &entry.hash).collect::<Vec<_>>(),
        vec![&newer, &older],
        "persisted first-ingest order survives reopen"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn legacy_sidecars_restore_at_zero_with_deterministic_hash_ties() {
    let root = temp_root("listing-legacy-order");
    let (left, right) = {
        let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
        let left = store.upload(b"legacy-left", ArtifactKind::Binary, manifest("headless"), Some("left".to_owned()));
        let right = store.upload(b"legacy-right", ArtifactKind::Binary, manifest("headless"), Some("right".to_owned()));
        (left, right)
    };
    for hash in [&left, &right] {
        let path = root.join("entries").join(format!("{hash}.manifest"));
        let mut sidecar: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read sidecar")).expect("decode sidecar JSON");
        sidecar.as_object_mut().expect("sidecar is an object").remove("uploaded_seq");
        fs::write(&path, serde_json::to_vec(&sidecar).expect("encode legacy sidecar")).expect("write legacy sidecar");
    }

    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let mut legacy_hashes = vec![left, right];
    legacy_hashes.sort();
    let legacy_page = store.list_binaries_page(&binary_page_filter(None, false));
    assert_eq!(
        legacy_page.binaries.iter().map(|entry| entry.hash.clone()).collect::<Vec<_>>(),
        legacy_hashes,
        "legacy sequence-zero ties sort by hash"
    );

    let fresh = store.upload(b"fresh", ArtifactKind::Binary, manifest("headless"), Some("fresh".to_owned()));
    let page = store.list_binaries_page(&binary_page_filter(None, false));
    assert_eq!(page.binaries[0].hash, fresh, "a post-restore ingest receives the next sequence");
    assert_eq!(page.binaries[1..].iter().map(|entry| entry.hash.clone()).collect::<Vec<_>>(), legacy_hashes);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multiple_names_share_one_row_with_the_smallest_representative() {
    let root = temp_root("listing-multi-name");
    let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("open store");
    let hash = store.upload(b"same", ArtifactKind::Binary, manifest("headless"), Some("zeta".to_owned()));
    assert_eq!(store.upload(b"same", ArtifactKind::Binary, manifest("headless"), Some("alpha".to_owned())), hash);

    let page = store.list_binaries_page(&binary_page_filter(None, false));
    assert_eq!(page.total_matched, 1, "one content hash remains one listing row");
    assert_eq!(page.binaries[0].name.as_deref(), Some("alpha"));
    let _ = fs::remove_dir_all(&root);
}
