//! `FleetBench` hub binary-store proof (ADR-0115, issue 1953): drive the
//! real hub → RPC → engines-cap stack to upload a binary
//! content-addressed, capture its `--describe` manifest, list it back,
//! dedup an identical re-upload, and resolve its name.

mod tests {
    use aether_kinds::{ListEngineBinaries, UploadBinaryResult};

    use aether_fleet_bench::FleetBench;

    /// Upload the real `aether-substrate-headless` binary, then assert
    /// the store ingested it content-addressed with the right
    /// `--describe` manifest, dedups an identical re-upload to the same
    /// hash, and resolves the name back.
    #[test]
    fn fleetbench_uploads_lists_and_dedups_a_real_binary() {
        let headless = env!("CARGO_BIN_EXE_aether-substrate-headless");
        let mut bench = FleetBench::start();

        // Upload + capture the manifest via the hub's one-time fork of
        // `<binary> --describe`.
        let hash = match bench.upload_binary(headless, Some("headless")) {
            UploadBinaryResult::Ok { hash, name } => {
                assert_eq!(name.as_deref(), Some("headless"), "the upload's name is echoed");
                assert!(!hash.is_empty(), "the content hash is non-empty");
                hash
            }
            UploadBinaryResult::Err { error } => panic!("upload_binary failed: {error}"),
        };

        let desktop = env!("CARGO_BIN_EXE_aether-substrate");
        let unnamed_hash = match bench.upload_binary(desktop, None) {
            UploadBinaryResult::Ok { hash, name } => {
                assert!(name.is_none(), "the historical fixture is deliberately unnamed");
                hash
            }
            UploadBinaryResult::Err { error } => panic!("uploading unnamed desktop binary failed: {error}"),
        };

        // The default is a named-only page: the headless entry is present
        // with the manifest its `--describe` fork captured, while the
        // distinct unnamed desktop upload is history and stays out.
        let listed = bench.list_engine_binaries(&ListEngineBinaries::default());
        assert_eq!(listed.total_matched, 1, "only the name-pointed registry entry matches by default");
        assert!(!listed.binaries.iter().any(|entry| entry.hash == unnamed_hash));
        let entry = listed
            .binaries
            .iter()
            .find(|e| e.hash == hash)
            .unwrap_or_else(|| panic!("uploaded binary {hash} should be listed: {:?}", listed.binaries));
        assert_eq!(entry.manifest.chassis, "headless", "the stored manifest reports the headless chassis");
        assert!(
            !entry.manifest.caps.is_empty(),
            "the stored manifest carries a non-empty cap list, got {:?}",
            entry.manifest.caps,
        );
        assert_eq!(entry.name.as_deref(), Some("headless"), "the name points at the entry");

        // A chassis filter that matches keeps it; one that doesn't drops it.
        let history =
            bench.list_engine_binaries(&ListEngineBinaries { include_history: true, ..ListEngineBinaries::default() });
        assert_eq!(history.total_matched, 2, "history opt-in includes the unnamed hash across RPC");
        assert!(history.binaries.iter().any(|entry| entry.hash == unnamed_hash));
        let zero = bench.list_engine_binaries(&ListEngineBinaries {
            limit: Some(0),
            include_history: true,
            ..ListEngineBinaries::default()
        });
        assert!(zero.binaries.is_empty(), "an explicit zero limit survives the RPC boundary");
        assert_eq!(zero.total_matched, 2, "the pre-cap match count survives the RPC boundary");

        let headless_filtered = bench.list_engine_binaries(&ListEngineBinaries {
            chassis: Some("headless".to_owned()),
            caps: vec![],
            target: None,
            limit: None,
            include_history: true,
        });
        assert!(headless_filtered.binaries.iter().any(|e| e.hash == hash), "a matching chassis filter keeps the entry");
        let desktop_filtered = bench.list_engine_binaries(&ListEngineBinaries {
            chassis: Some("desktop".to_owned()),
            caps: vec![],
            target: None,
            limit: None,
            include_history: true,
        });
        assert!(
            !desktop_filtered.binaries.iter().any(|e| e.hash == hash),
            "a non-matching chassis filter drops the entry"
        );

        // A second identical upload dedups to the same content hash.
        let again = match bench.upload_binary(headless, None) {
            UploadBinaryResult::Ok { hash, .. } => hash,
            UploadBinaryResult::Err { error } => panic!("re-upload failed: {error}"),
        };
        assert_eq!(again, hash, "an identical re-upload dedups to the same hash");
    }
}
