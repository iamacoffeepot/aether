mod fleetbench;

mod tests {
    use std::path::Path;

    use crate::fleetbench::{
        DistComponentGuardOutcome, DistManifestClassification, classify_dist_manifest,
        dist_component_guard_outcome,
    };

    #[test]
    fn manifest_with_bundle_stem_is_available() {
        let manifest = r#"{
            "components": {
                "aether_test_fixtures_bundle": "components/aether_test_fixtures_bundle.wasm"
            }
        }"#;
        assert!(matches!(
            classify_dist_manifest(manifest, "aether_test_fixtures_bundle"),
            DistManifestClassification::Available { .. }
        ));
    }

    #[test]
    fn manifest_missing_bundle_stem_is_classified() {
        let manifest = r#"{
            "components": {
                "aether_kit": "components/aether_kit.wasm"
            }
        }"#;
        assert_eq!(
            classify_dist_manifest(manifest, "aether_test_fixtures_bundle"),
            DistManifestClassification::MissingStem,
        );
    }

    #[test]
    fn missing_stem_skips_locally_and_panics_when_runtime_is_required() {
        let manifest_path = Path::new("/tmp/dist/manifest.json");
        let local = dist_component_guard_outcome(
            "aether_test_fixtures_bundle",
            manifest_path,
            &DistManifestClassification::MissingStem,
            false,
        );
        let required = dist_component_guard_outcome(
            "aether_test_fixtures_bundle",
            manifest_path,
            &DistManifestClassification::MissingStem,
            true,
        );

        let DistComponentGuardOutcome::Skip(skip_message) = local else {
            panic!("missing stem should skip locally");
        };
        let DistComponentGuardOutcome::RequireRuntime(panic_message) = required else {
            panic!("missing stem should require runtime under AETHER_REQUIRE_RUNTIME");
        };
        assert_eq!(skip_message, panic_message);
        assert!(skip_message.contains("aether_test_fixtures_bundle"));
        assert!(skip_message.contains("/tmp/dist/manifest.json"));
        assert!(skip_message.contains("cargo xtask dist"));
        assert!(skip_message.contains("remove generated `dist/`"));
    }
}
