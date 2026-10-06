//! ADR-0163 §3 (#3984) proof: a wasm guest actually pulls an asset through
//! its load window during `wire`. Loads the fixture bundle (which
//! `export_asset!`s `asset_fixture.txt`) selecting its `QuietProbe` export,
//! which in `wire` pulls the asset through `AssetWindow::asset` and stashes a
//! fingerprint, then sends `AssetProbe` over the wire and asserts the reply
//! carries the exact bytes' length and content checksum — proving the guest-side `asset_fetch_p32`
//! transport round-tripped the payload, and that it survived the window
//! closing (the probe reply runs in an ordinary post-`wire` handler).
//!
//! The same `wire` takes the asset as a blob through
//! `AssetWindow::asset_blob` and keeps it; `AssetBlobProbe` is answered with
//! that blob, which reaches the session as the asset's exact bytes.
//!
//! A spawn reads its module's assets only from the code it brings (issue
//! #7461): a boot manifest's instances and a `Spawn` carrying `code` pull
//! the exact bytes, a spawn bringing another module's bytes is refused, and
//! one bringing none traps on a catalogued asset.

mod tests {
    use std::env;
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    use aether_data::{Blob, EngineId, Kind};
    use aether_kinds::{LoadComponent, Spawn, SpawnResult};
    use aether_test_fixtures_kinds::{
        AssetBlobProbe, AssetBlobProbeResult, AssetProbe, AssetProbeResult, EmptyAssetProbe, EmptyAssetProbeResult,
    };

    use aether_harness_fleet::{FleetHarness, component_wasm_path, dist_component_available, read_component_wasm};

    const BUNDLE: &str = "aether_test_fixtures_bundle";
    const QUIET_PROBE: &str = "test.quiet_probe";
    const ASSET_INSTANCE: &str = "test.asset_instance";

    /// The source asset the bundle embeds via
    /// `export_asset!("asset_fixture.txt")`, read at compile time so the
    /// length + checksum assertions are computed tripwires against the exact
    /// bytes the guest should receive.
    const ASSET_FIXTURE: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../aether-test-fixtures-bundle/src/asset_fixture.txt"));

    /// The same wrapping-sum checksum the fixture computes in `wire`, over
    /// the source bytes — a content-sensitive fingerprint, so a corrupt or
    /// truncated FFI transfer reds this rather than passing on length alone.
    fn checksum(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0u64, |acc, &byte| acc.wrapping_add(u64::from(byte)))
    }

    /// Load `test.quiet_probe` from the fixture bundle's bytes and return
    /// the fingerprint of the asset its `wire` pulled through the window.
    fn load_quiet_probe(harness: &mut FleetHarness, engine: EngineId) -> AssetProbeResult {
        let load = LoadComponent {
            wasm: read_component_wasm(BUNDLE),
            name: None,
            config: Vec::new(),
            export: Some(QUIET_PROBE.to_owned()),
        };
        let addr = harness.load(engine, &load).addr;
        probe(harness, engine, &addr)
    }

    fn probe(harness: &mut FleetHarness, engine: EngineId, addr: &str) -> AssetProbeResult {
        let replies = harness.send(engine, addr, &AssetProbe);
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("asset_probe expected exactly one reply event, got {}", other.len()),
        };
        assert_eq!(reply.kind, AssetProbeResult::ID, "the reply should be an AssetProbeResult");
        AssetProbeResult::decode_from_bytes(&reply.payload).expect("the reply payload decodes as AssetProbeResult")
    }

    fn assert_pulled_exact(result: &AssetProbeResult) {
        assert!(result.pulled, "the guest's `wire` must have pulled the asset through the window, got {result:?}");
        assert_eq!(
            result.len,
            ASSET_FIXTURE.len() as u64,
            "the pulled asset length must match the embedded source bytes",
        );
        assert_eq!(
            result.checksum,
            checksum(ASSET_FIXTURE),
            "the pulled asset content checksum must match the source — the exact bytes crossed the FFI",
        );
    }

    #[test]
    fn fleetharness_guest_pulls_asset_through_the_window() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        assert_pulled_exact(&load_quiet_probe(&mut harness, engine));
    }

    /// The blob a guest took from its load window and kept reaches a session
    /// as the asset's exact bytes over the real hub path, from a handler
    /// that runs after the window closed. It catches an `asset_blob` whose
    /// hold the guest's value does not own (the reply's hash resolving to
    /// nothing once `wire` returned), a view over the wrong range of the
    /// module file, and a reply that leaves the process as a hash.
    #[test]
    fn fleetharness_guest_forwards_an_asset_blob_it_took_through_the_window() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let load = LoadComponent {
            wasm: read_component_wasm(BUNDLE),
            name: None,
            config: Vec::new(),
            export: Some(QUIET_PROBE.to_owned()),
        };
        let addr = harness.load(engine, &load).addr;

        let replies = harness.send(engine, &addr, &AssetBlobProbe);

        let [reply] = replies.as_slice() else {
            panic!("asset_blob_probe expected exactly one reply event, got {}", replies.len());
        };
        assert_eq!(reply.kind, AssetBlobProbeResult::ID, "the reply should be an AssetBlobProbeResult");
        let blob = AssetBlobProbeResult::decode_from_bytes(&reply.payload)
            .expect("the reply payload decodes as AssetBlobProbeResult")
            .blob
            .expect("the guest's `wire` took the asset as a blob");
        assert_eq!(blob.contiguous(), Some(ASSET_FIXTURE), "the exact asset bytes reached the session");
    }

    /// The fixture bundle's bytes with one more custom section appended:
    /// `aether.asset.asset_empty.bin`, carrying no payload (section id 0, the
    /// section length, the name length, the name).
    fn bundle_with_empty_asset() -> Vec<u8> {
        let name = b"aether.asset.asset_empty.bin";
        let name_len = u8::try_from(name.len()).expect("the section name fits a one-byte length");
        let mut wasm = read_component_wasm(BUNDLE);
        wasm.extend([0, name_len + 1, name_len]);
        wasm.extend(name);
        wasm
    }

    fn empty_probe(harness: &mut FleetHarness, engine: EngineId, wasm: Vec<u8>) -> EmptyAssetProbeResult {
        let load = LoadComponent { wasm, name: None, config: Vec::new(), export: Some(QUIET_PROBE.to_owned()) };
        let addr = harness.load(engine, &load).addr;

        let replies = harness.send(engine, &addr, &EmptyAssetProbe);

        let [reply] = replies.as_slice() else {
            panic!("empty_asset_probe expected exactly one reply event, got {}", replies.len());
        };
        assert_eq!(reply.kind, EmptyAssetProbeResult::ID, "the reply should be an EmptyAssetProbeResult");
        EmptyAssetProbeResult::decode_from_bytes(&reply.payload)
            .expect("the reply payload decodes as EmptyAssetProbeResult")
    }

    /// A zero-length asset reads as empty by both verbs, and an asset the
    /// module does not carry still reads as missing. It catches the host
    /// trapping on an empty delivery (the load itself fails, because `wire`
    /// traps), an empty asset answered as missing by either verb, a blob
    /// path that refuses an empty range, and an absent asset answered as an
    /// empty one.
    #[test]
    fn fleetharness_a_zero_length_asset_reads_as_empty() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        let empty = empty_probe(&mut harness, engine, bundle_with_empty_asset());
        let other_engine = harness.spawn_headless();
        let absent = empty_probe(&mut harness, other_engine, read_component_wasm(BUNDLE));

        assert_eq!(empty, EmptyAssetProbeResult { copied_len: Some(0), blob_len: Some(0) });
        assert_eq!(absent, EmptyAssetProbeResult { copied_len: None, blob_len: None });
    }

    /// A module published first keeps no asset payload (ADR-0163 §3), so a
    /// later load of the same bytes, a module-cache hit, must read its
    /// assets from the code that load brought. It catches the load door not
    /// handing its code to the guest's window when the module was already
    /// checked in.
    #[test]
    fn fleetharness_a_load_after_publish_reads_its_assets() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        harness.publish(engine, read_component_wasm(BUNDLE));

        assert_pulled_exact(&load_quiet_probe(&mut harness, engine));
    }

    /// A spawn that brings no bytes opens a window with no code, so the
    /// guest's fetch of a catalogued asset in `wire` traps naming what the
    /// spawn lacked (ADR-0163 §4) instead of reading as a missing asset. The
    /// trap fails the birth (ADR-0247 rule 3), so the spawn answers `Err`
    /// with that reason and stands nothing up, and the name is free for a
    /// spawn that brings the bytes. The fixture's first fetch is
    /// `asset_blob`, so that is the verb that traps here, under the window
    /// check it shares with `asset`. It catches a sourceless window answering
    /// a catalogued asset with a silent `None`, by either verb, and a trap
    /// in the load window that leaves the instance live and answers the
    /// spawn `Spawned`.
    #[test]
    fn fleetharness_a_spawned_instance_cannot_read_assets_without_its_bytes() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        harness.publish(engine, read_component_wasm(BUNDLE));

        let sourceless =
            Spawn { namespace: QUIET_PROBE.to_owned(), key: None, parent: None, config: Vec::new(), code: None };
        let replies = harness.send(engine, "aether.component", &sourceless);

        let [reply] = replies.as_slice() else {
            panic!("the spawn expected exactly one reply event, got {}", replies.len());
        };
        let result = SpawnResult::decode_from_bytes(&reply.payload).expect("the reply payload decodes as SpawnResult");
        let SpawnResult::Err { error } = result else {
            panic!("a spawn whose guest traps in wire is refused, got {result:?}");
        };
        let names_verb = error.contains("asset_blob");
        let names_cause = error.contains("spawned without its module's bytes");
        assert!(names_verb && names_cause, "the refusal names the fetch that trapped and why, got: {error}");
        let names = harness.list_components(engine);
        assert!(!names.iter().any(|name| name == QUIET_PROBE), "the failed spawn stood nothing up: {names:?}");

        let sourced = Spawn { code: Some(Blob::from(read_component_wasm(BUNDLE))), ..sourceless };
        let SpawnResult::Spawned { path, .. } = harness.spawn(engine, &sourced) else {
            panic!("the name is free, so a spawn bringing its code stands the instance up");
        };
        assert_pulled_exact(&probe(&mut harness, engine, &path.to_string()));
    }

    /// Every instance a boot manifest stands up reads its module's assets in
    /// `wire`: a singleton entry, and both instances of a `replicas: 2`
    /// entry. The engine serves only after every boot entry answered, so
    /// each probe is answered on the first call. It catches a boot instance
    /// whose load window has no bytes (issue #7461), and a boot loader that
    /// hands the code to the first key's spawn only.
    #[test]
    fn fleetharness_boot_manifest_instances_read_their_assets() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let wasm = component_wasm_path(BUNDLE);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        let manifest_path = env::temp_dir().join(format!("aether-asset-window-boot-{}-{nanos}.json", process::id()));
        let manifest = serde_json::json!({
            "components": [
                { "wasm": wasm.to_string_lossy(), "export": QUIET_PROBE },
                { "wasm": wasm.to_string_lossy(), "export": ASSET_INSTANCE, "replicas": 2 },
            ],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("serialize the boot manifest"))
            .expect("write the boot manifest");
        let mut harness = FleetHarness::start();

        let engine = harness.spawn_headless_with_boot_manifest(&manifest_path);

        for addr in [QUIET_PROBE.to_owned(), format!("{ASSET_INSTANCE}:0"), format!("{ASSET_INSTANCE}:1")] {
            assert_pulled_exact(&probe(&mut harness, engine, &addr));
        }
        let _ = fs::remove_file(&manifest_path);
    }

    /// A spawn that brings the published module's bytes reads its assets,
    /// here with the bytes inline in the mail, the form a blob takes over
    /// RPC. It catches the host dropping a spawn's code before the guest's
    /// load window opens.
    #[test]
    fn fleetharness_a_spawn_bringing_its_code_reads_its_assets() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        harness.publish(engine, read_component_wasm(BUNDLE));

        let spawn = Spawn {
            namespace: QUIET_PROBE.to_owned(),
            key: None,
            parent: None,
            config: Vec::new(),
            code: Some(Blob::from(read_component_wasm(BUNDLE))),
        };
        let SpawnResult::Spawned { path, .. } = harness.spawn(engine, &spawn) else {
            panic!("a fresh spawn of {QUIET_PROBE} stands the instance up");
        };

        assert_pulled_exact(&probe(&mut harness, engine, &path.to_string()));
    }

    /// A spawn that brings a different module over the same code, the
    /// bundle with one more asset section, is refused and stands nothing
    /// up. It catches a load window opened over bytes whose asset ranges
    /// belong to another module, which would serve the wrong bytes.
    #[test]
    fn fleetharness_a_spawn_bringing_other_code_is_refused() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        harness.publish(engine, read_component_wasm(BUNDLE));

        let spawn = Spawn {
            namespace: QUIET_PROBE.to_owned(),
            key: None,
            parent: None,
            config: Vec::new(),
            code: Some(Blob::from(bundle_with_empty_asset())),
        };
        let replies = harness.send(engine, "aether.component", &spawn);

        let [reply] = replies.as_slice() else {
            panic!("the spawn expected exactly one reply event, got {}", replies.len());
        };
        let result = SpawnResult::decode_from_bytes(&reply.payload).expect("the reply payload decodes as SpawnResult");
        let SpawnResult::Err { error } = result else {
            panic!("a spawn bringing another module's bytes is refused, got {result:?}");
        };
        let names_namespace = error.contains(QUIET_PROBE);
        let names_mismatch = error.contains("not the module that publishes it");
        assert!(names_namespace && names_mismatch, "the refusal names the namespace and the mismatch, got: {error}");
        let names = harness.list_components(engine);
        assert!(!names.iter().any(|name| name == QUIET_PROBE), "the refused spawn stood nothing up: {names:?}");
    }
}
