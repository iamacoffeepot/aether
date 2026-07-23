//! ADR-0163 §3 (#3984) proof: a wasm guest actually pulls an asset through
//! its load window during `wire`. Loads the fixture bundle (whose `Probe`
//! entry actor `export_asset!`s `asset_fixture.txt` and, in `wire`, pulls it
//! through `AssetWindow::asset` and stashes a fingerprint), then sends
//! `AssetProbe` over the wire and asserts the reply carries the exact bytes'
//! length and content checksum — proving the guest-side `asset_fetch_p32`
//! transport round-tripped the payload, and that it survived the window
//! closing (the probe reply runs in an ordinary post-`wire` handler).

mod tests {
    use aether_data::Kind;
    use aether_test_fixtures_kinds::{AssetProbe, AssetProbeResult};

    use aether_harness_fleet::{FleetHarness, dist_component_available};

    /// The source asset the bundle embeds via
    /// `export_asset!("asset_fixture.txt")`, read at compile time so the
    /// length + checksum assertions are computed tripwires against the exact
    /// bytes the guest should receive.
    const ASSET_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../aether-test-fixtures/aether-test-fixtures-bundle/src/asset_fixture.txt"
    ));

    /// The same wrapping-sum checksum the fixture computes in `wire`, over
    /// the source bytes — a content-sensitive fingerprint, so a corrupt or
    /// truncated FFI transfer reds this rather than passing on length alone.
    fn checksum(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0u64, |acc, &byte| acc.wrapping_add(u64::from(byte)))
    }

    #[test]
    fn fleetharness_guest_pulls_asset_through_the_window() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let addr = harness.load(engine, "aether_test_fixtures_bundle");

        let replies = harness.send(engine, &addr, &AssetProbe);
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("asset_probe expected exactly one reply event, got {}", other.len()),
        };
        assert_eq!(reply.kind, AssetProbeResult::ID, "the reply should be an AssetProbeResult");
        let result =
            AssetProbeResult::decode_from_bytes(&reply.payload).expect("the reply payload decodes as AssetProbeResult");

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
}
