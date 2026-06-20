//! `FleetBench` hub component-store proof (ADR-0116, issue 1956): drive the
//! real hub → RPC → engines-cap stack to upload a component wasm
//! content-addressed, read its manifest straight from the wasm, resolve it
//! by name / hash / handled-kind attribute, dedup an identical re-upload,
//! load + replace by selector, and bring a component up from a boot manifest
//! of selectors. Headless: no GPU, no pixel readback.

mod fleetbench;

mod tests {
    use std::env;
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    use aether_actor::Addressable;
    use aether_capabilities::WasmTrampoline;
    use aether_data::Kind;
    use aether_kinds::{
        ComponentSelector, ListComponentBinaries, LogTailResult, ResolveComponentResult, Tick,
        UploadComponentResult,
    };

    use crate::fleetbench::{FleetBench, component_wasm_path, dist_manifest_present, poll_until};

    /// The probe fixture's declared `Addressable::NAMESPACE` (distinct from the
    /// `probe` wasm stem).
    const PROBE_NAMESPACE: &str = "test_fixture_probe";

    /// The probe's registered ADR-0099 lineage address.
    fn probe_lineage_addr() -> String {
        format!(
            "aether.component/{}:{PROBE_NAMESPACE}",
            WasmTrampoline::NAMESPACE,
        )
    }

    /// Resolve `selector` hub-local and return the matched content hash,
    /// panicking on an `Err` (no match / ambiguity).
    fn resolve_hash(bench: &mut FleetBench, selector: ComponentSelector) -> String {
        match bench.resolve_component(selector) {
            ResolveComponentResult::Ok { hash, .. } => hash,
            ResolveComponentResult::Err { error } => panic!("resolve failed: {error}"),
        }
    }

    /// Upload the probe by staged path and assert the store ingested it
    /// content-addressed with a manifest read from the wasm (no execution
    /// step): the probe's namespace + handled `Tick`, deduping an
    /// identical re-upload. Returns the content hash.
    fn upload_and_assert_manifest(bench: &mut FleetBench, probe_path: &str) -> String {
        let hash = match bench.upload_component(probe_path, Some("probe")) {
            UploadComponentResult::Ok { hash, name } => {
                assert_eq!(
                    name.as_deref(),
                    Some("probe"),
                    "the upload's name is echoed"
                );
                assert!(!hash.is_empty(), "the content hash is non-empty");
                hash
            }
            UploadComponentResult::Err { error } => panic!("upload_component failed: {error}"),
        };

        let all = bench.list_component_binaries(&ListComponentBinaries::default());
        let entry = all
            .iter()
            .find(|e| e.hash == hash)
            .unwrap_or_else(|| panic!("uploaded component {hash} should be listed: {all:?}"));
        assert!(
            entry
                .manifest
                .namespaces
                .iter()
                .any(|n| n == PROBE_NAMESPACE),
            "the manifest reports the probe's namespace, got {:?}",
            entry.manifest.namespaces,
        );
        assert!(
            entry.manifest.handled_kinds.contains(&Tick::ID),
            "the manifest reports the probe handles Tick",
        );
        assert_eq!(
            entry.name.as_deref(),
            Some("probe"),
            "the name points at it"
        );

        // Attribute filters: namespace + handled-kind keep it, a miss drops it.
        assert!(
            bench
                .list_component_binaries(&ListComponentBinaries {
                    namespace: Some(PROBE_NAMESPACE.to_owned()),
                    handled_kind: None,
                })
                .iter()
                .any(|e| e.hash == hash),
            "a matching namespace filter keeps the entry",
        );
        assert!(
            bench
                .list_component_binaries(&ListComponentBinaries {
                    namespace: None,
                    handled_kind: Some(Tick::ID),
                })
                .iter()
                .any(|e| e.hash == hash),
            "a matching handled-kind filter keeps the entry",
        );
        assert!(
            !bench
                .list_component_binaries(&ListComponentBinaries {
                    namespace: Some("not_a_namespace".to_owned()),
                    handled_kind: None,
                })
                .iter()
                .any(|e| e.hash == hash),
            "a non-matching namespace filter drops the entry",
        );

        // A second identical upload dedups to the same content hash.
        let again = match bench.upload_component(probe_path, None) {
            UploadComponentResult::Ok { hash, .. } => hash,
            UploadComponentResult::Err { error } => panic!("re-upload failed: {error}"),
        };
        assert_eq!(
            again, hash,
            "an identical re-upload dedups to the same hash"
        );
        hash
    }

    /// Upload the probe component by staged path, then assert the store
    /// ingested it content-addressed with a manifest read from the wasm,
    /// resolves + loads it by name / hash / handled-kind, dedups an
    /// identical re-upload, and replaces by hash.
    #[test]
    fn fleetbench_uploads_resolves_loads_and_replaces_a_component() {
        if !dist_manifest_present() {
            return;
        }
        let probe_path = component_wasm_path("aether_test_fixtures_bundle")
            .to_string_lossy()
            .into_owned();
        let mut bench = FleetBench::start();

        let hash = upload_and_assert_manifest(&mut bench, &probe_path);

        // Resolve the same component three ways — by name, by hash, and
        // by a handled-kind attribute — each lands on the probe hash. (A
        // single namespace can only be loaded once per engine, so the
        // three selectors are proven equivalent at the resolve hop, then
        // the one load below proves resolve-and-forward end to end.)
        assert_eq!(
            resolve_hash(
                &mut bench,
                ComponentSelector {
                    query: Some("probe".to_owned()),
                    namespace: None,
                    handled_kind: None,
                }
            ),
            hash,
            "the name selector resolves to the probe hash",
        );
        assert_eq!(
            resolve_hash(
                &mut bench,
                ComponentSelector {
                    query: Some(hash.clone()),
                    namespace: None,
                    handled_kind: None,
                }
            ),
            hash,
            "the hash selector resolves to the probe hash",
        );
        assert_eq!(
            resolve_hash(
                &mut bench,
                ComponentSelector {
                    query: None,
                    namespace: None,
                    handled_kind: Some(Tick::ID),
                }
            ),
            hash,
            "the handled-kind attribute selector resolves to the probe hash",
        );

        // Fork a headless engine, load by selector (resolve-and-forward),
        // and assert it registers at the lineage address and answers
        // LogTail (it's live).
        let engine = bench.spawn_headless();
        let expected = probe_lineage_addr();
        let loaded = bench.load_by_selector(engine, "probe");
        assert_eq!(
            loaded.addr, expected,
            "load by selector registers at the lineage addr"
        );
        match bench.log_tail(engine, &expected, None) {
            LogTailResult::Ok { .. } => {}
            LogTailResult::Err { error } => {
                panic!("the loaded probe should answer LogTail, got Err: {error}")
            }
        }

        // Replace the loaded component by hash (ADR-0022 in-place swap,
        // ADR-0116 selector). The trampoline keeps its lineage address.
        let caps = bench.replace_by_selector(engine, loaded.mailbox_id, &hash);
        assert!(
            caps.handlers.iter().any(|h| h.id == Tick::ID),
            "the replaced probe still advertises its Tick handler",
        );
    }

    /// A `spawn_substrate` boot manifest written in component selectors
    /// brings the component set up reproducibly: aether-mcp pre-resolves
    /// each selector to bytes and stages a path-based manifest the
    /// substrate reads at boot (ADR-0116). `FleetBench` mirrors that
    /// pre-resolution (it speaks raw frames, not aether-mcp): upload,
    /// resolve hub-local, stage the bytes, and spawn with the manifest.
    #[test]
    fn fleetbench_boots_a_component_set_from_a_selector_manifest() {
        if !dist_manifest_present() {
            return;
        }
        let probe_path = component_wasm_path("aether_test_fixtures_bundle")
            .to_string_lossy()
            .into_owned();
        let mut bench = FleetBench::start();

        let hash = match bench.upload_component(&probe_path, Some("probe")) {
            UploadComponentResult::Ok { hash, .. } => hash,
            UploadComponentResult::Err { error } => panic!("upload_component failed: {error}"),
        };

        // Pre-resolve the selector hub-local to the wasm bytes (the
        // aether-mcp boot-manifest pre-resolution hop), stage them to a
        // temp wasm file, and write a path-based boot manifest pointing at
        // it — the same shape aether-mcp's `stage_boot_manifest` produces.
        let wasm = match bench.resolve_component(ComponentSelector {
            query: Some(hash),
            namespace: None,
            handled_kind: None,
        }) {
            ResolveComponentResult::Ok { wasm, .. } => wasm,
            ResolveComponentResult::Err { error } => {
                panic!("resolve for boot manifest failed: {error}")
            }
        };
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let staged_wasm =
            env::temp_dir().join(format!("aether-fb-boot-{}-{nanos}.wasm", process::id()));
        fs::write(&staged_wasm, &wasm).expect("stage the resolved boot wasm");
        let manifest_path =
            env::temp_dir().join(format!("aether-fb-manifest-{}-{nanos}.json", process::id()));
        // No explicit `name`: the trampoline registers at the
        // namespace-derived ADR-0099 lineage address, matching the load
        // path and `probe_lineage_addr()`.
        let manifest_json = serde_json::json!({
            "components": [{ "wasm": staged_wasm.to_string_lossy() }],
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest_json).expect("serialize boot manifest"),
        )
        .expect("write boot manifest");

        // Spawn with the boot manifest; the substrate reads it at boot.
        let engine = bench.spawn_headless_with_boot_manifest(&manifest_path);

        // The boot autoload is async, so poll the engine's loaded-components
        // query (issue 2020) until the probe's lineage address appears. This
        // is the deterministic registration edge: `aether.component.list`
        // reflects the live trampoline set, so the probe's name is present
        // exactly when it is loaded and registered — no log-ring side channel
        // and no racing a fixed liveness budget.
        let expected = probe_lineage_addr();
        let registered =
            poll_until(|| bench.list_components(engine).iter().any(|n| n == &expected));
        assert!(
            registered,
            "the boot-manifest probe should come up and register at {expected}",
        );

        // Best-effort: clean up the staged temp files.
        let _ = fs::remove_file(&staged_wasm);
        let _ = fs::remove_file(&manifest_path);
    }
}
