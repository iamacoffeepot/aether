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

mod tests {
    use aether_data::{EngineId, Kind};
    use aether_kinds::{LoadComponent, LogTailResult, Spawn, SpawnResult};
    use aether_test_fixtures_kinds::{AssetBlobProbe, AssetBlobProbeResult, AssetProbe, AssetProbeResult};

    use aether_harness_fleet::{FleetHarness, dist_component_available, read_component_wasm};

    const BUNDLE: &str = "aether_test_fixtures_bundle";
    const QUIET_PROBE: &str = "test.quiet_probe";

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

    /// A spawn from a publication brings no bytes, so the guest's fetch of a
    /// catalogued asset in `wire` traps naming `load_component` (ADR-0163
    /// §4) instead of reading as a missing asset. The fixture's first fetch
    /// is `asset_blob`, so that is the verb that traps here, under the
    /// window check it shares with `asset`. It catches a sourceless window
    /// answering a catalogued asset with a silent `None`, by either verb.
    #[test]
    fn fleetharness_a_spawned_instance_cannot_read_assets_without_its_bytes() {
        if !dist_component_available(BUNDLE) {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        harness.publish(engine, read_component_wasm(BUNDLE));

        let spawn = Spawn { namespace: QUIET_PROBE.to_owned(), key: None, parent: None, config: Vec::new() };
        let SpawnResult::Spawned { path, .. } = harness.spawn(engine, &spawn) else {
            panic!("a fresh spawn of {QUIET_PROBE} stands the instance up");
        };
        let addr = path.to_string();

        assert!(!probe(&mut harness, engine, &addr).pulled, "a spawned instance reads no asset bytes");
        let LogTailResult::Ok { entries, .. } =
            harness.log_tail(engine, &addr, None, Some("load_component".to_owned()))
        else {
            panic!("the spawned instance answers LogTail");
        };
        let trapped_on_blob = entries.iter().any(|entry| entry.message.contains("asset_blob"));
        assert!(trapped_on_blob, "the guest's `wire` asset_blob trapped naming load_component, got {entries:?}");
    }
}
