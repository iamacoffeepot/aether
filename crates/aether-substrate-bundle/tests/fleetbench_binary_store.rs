//! `FleetBench` hub binary-store proof (ADR-0115, issue 1953): drive the
//! real hub → RPC → engines-cap stack to upload a binary
//! content-addressed, capture its `--describe` manifest, list it back,
//! dedup an identical re-upload, and resolve its name.
//!
//! Heavy by construction — it boots a hub chassis and the engines cap
//! forks `<binary> --describe` to capture the manifest — so the test lives
//! in `mod tests::heavy`, which nextest serializes (`serial-heavy`).

mod fleetbench;

mod tests {
    mod heavy {
        use aether_kinds::{ListBinaries, UploadBinaryResult};

        use crate::fleetbench::FleetBench;

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
                    assert_eq!(
                        name.as_deref(),
                        Some("headless"),
                        "the upload's name is echoed"
                    );
                    assert!(!hash.is_empty(), "the content hash is non-empty");
                    hash
                }
                UploadBinaryResult::Err { error } => panic!("upload_binary failed: {error}"),
            };

            // List with no filter — the entry is present with the headless
            // manifest the `--describe` fork captured.
            let all = bench.list_binaries(&ListBinaries::default());
            let entry = all
                .iter()
                .find(|e| e.hash == hash)
                .unwrap_or_else(|| panic!("uploaded binary {hash} should be listed: {all:?}"));
            assert_eq!(
                entry.manifest.chassis, "headless",
                "the stored manifest reports the headless chassis",
            );
            assert!(
                !entry.manifest.caps.is_empty(),
                "the stored manifest carries a non-empty cap list, got {:?}",
                entry.manifest.caps,
            );
            assert_eq!(
                entry.name.as_deref(),
                Some("headless"),
                "the name points at the entry"
            );

            // A chassis filter that matches keeps it; one that doesn't drops it.
            let headless_filtered = bench.list_binaries(&ListBinaries {
                chassis: Some("headless".to_owned()),
                caps: vec![],
                target: None,
            });
            assert!(
                headless_filtered.iter().any(|e| e.hash == hash),
                "a matching chassis filter keeps the entry",
            );
            let desktop_filtered = bench.list_binaries(&ListBinaries {
                chassis: Some("desktop".to_owned()),
                caps: vec![],
                target: None,
            });
            assert!(
                !desktop_filtered.iter().any(|e| e.hash == hash),
                "a non-matching chassis filter drops the entry",
            );

            // A second identical upload dedups to the same content hash.
            let again = match bench.upload_binary(headless, None) {
                UploadBinaryResult::Ok { hash, .. } => hash,
                UploadBinaryResult::Err { error } => panic!("re-upload failed: {error}"),
            };
            assert_eq!(
                again, hash,
                "an identical re-upload dedups to the same hash"
            );
        }
    }
}
