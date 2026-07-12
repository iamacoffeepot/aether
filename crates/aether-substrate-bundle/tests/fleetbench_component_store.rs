//! `FleetBench` hub component-store proof (ADR-0116, issue 1956): drive the
//! real hub → RPC → engines-cap stack to upload a component wasm
//! content-addressed, read its manifest straight from the wasm, resolve it
//! by name / hash / handled-kind attribute, dedup an identical re-upload,
//! load + replace by selector, and bring a component up from a boot manifest
//! of selectors. Headless: no GPU, no pixel readback.

mod fleetbench;

mod tests {
    use std::collections::BTreeSet;
    use std::env;
    use std::fs;
    use std::process;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use aether_actor::Addressable;
    use aether_capabilities::WasmTrampoline;
    use aether_data::{EngineId, Kind, Schema, SchemaType, wire};
    use aether_kinds::{
        ComponentSelector, ListComponentBinaries, LogTailResult, ResolveComponentResult, Tick, UploadComponentResult,
    };
    use aether_test_fixtures_kinds::ProbeConfig;

    use crate::fleetbench::{
        FleetBench, allocate_store_root_for_test, component_wasm_path, dist_component_available, poll_until,
    };

    /// The probe fixture's declared `Addressable::NAMESPACE` (distinct from the
    /// `probe` wasm stem).
    const PROBE_NAMESPACE: &str = "test.probe";
    /// `info` in the `0 = trace .. 4 = error` log-level mapping.
    const LEVEL_INFO: u8 = 2;
    /// One-shot log emitted by each fresh probe instance on its first tick.
    const PROBE_FIRST_TICK_LOG: &str = "typed_send_alive";

    /// The probe's registered ADR-0099 lineage address.
    fn probe_lineage_addr() -> String {
        format!("aether.component/{}:{PROBE_NAMESPACE}", WasmTrampoline::NAMESPACE)
    }

    /// Resolve `selector` hub-local and return the matched content hash,
    /// panicking on an `Err` (no match / ambiguity).
    fn resolve_hash(bench: &mut FleetBench, selector: ComponentSelector) -> String {
        match bench.resolve_component(selector) {
            ResolveComponentResult::Ok { hash, .. } => hash,
            ResolveComponentResult::Err { error } => panic!("resolve failed: {error}"),
        }
    }

    /// Poll one probe lineage ring from `since` until a fresh instance's
    /// one-shot first-tick entry appears. Returns the entry sequence and the
    /// ring cursor so a caller can require the next lifecycle observation to
    /// be strictly newer.
    fn poll_probe_first_tick(
        bench: &mut FleetBench,
        engine: EngineId,
        addr: &str,
        since: Option<u64>,
        lifecycle: &str,
    ) -> (u64, u64) {
        let mut last_reply = None;
        let mut found = None;
        poll_until(|| {
            let reply = bench.log_tail(engine, addr, since, Some(PROBE_FIRST_TICK_LOG.to_owned()));
            if let LogTailResult::Ok { entries, .. } = &reply {
                assert!(
                    entries.iter().all(|entry| entry.message.contains(PROBE_FIRST_TICK_LOG)),
                    "the substrate-side contains filter should remove non-matching entries: {entries:?}",
                );
            }
            if let LogTailResult::Ok { entries, next_since, .. } = &reply
                && let Some(entry) =
                    entries.iter().find(|entry| entry.message == PROBE_FIRST_TICK_LOG && entry.level == LEVEL_INFO)
            {
                found = Some((entry.sequence, *next_since));
                true
            } else {
                last_reply = Some(reply);
                false
            }
        });

        found.unwrap_or_else(|| {
            panic!(
                "probe's `{PROBE_FIRST_TICK_LOG}` entry never appeared after {lifecycle} within the poll budget; \
                 last reply: {last_reply:?}",
            )
        })
    }

    /// Upload the probe by staged path and assert the store ingested it
    /// content-addressed with a manifest read from the wasm (no execution
    /// step): the probe's namespace + handled `Tick`, deduping an
    /// identical re-upload. Returns the content hash.
    fn upload_and_assert_manifest(bench: &mut FleetBench, probe_path: &str) -> String {
        let hash = match bench.upload_component(probe_path, Some("probe")) {
            UploadComponentResult::Ok { hash, name } => {
                assert_eq!(name.as_deref(), Some("probe"), "the upload's name is echoed");
                assert!(!hash.is_empty(), "the content hash is non-empty");
                hash
            }
            UploadComponentResult::Err { error } => panic!("upload_component failed: {error}"),
        };

        let listed = bench.list_component_binaries(&ListComponentBinaries::default());
        assert_eq!(listed.total_matched, 1);
        let entry = listed
            .components
            .iter()
            .find(|e| e.hash == hash)
            .unwrap_or_else(|| panic!("uploaded component {hash} should be listed: {:?}", listed.components));
        assert!(
            entry.manifest.namespaces.iter().any(|n| n == PROBE_NAMESPACE),
            "the manifest reports the probe's namespace, got {:?}",
            entry.manifest.namespaces,
        );
        assert!(entry.manifest.handled_kinds.contains(&Tick::ID), "the manifest reports the probe handles Tick");
        assert_eq!(entry.name.as_deref(), Some("probe"), "the name points at it");

        // Attribute filters: namespace + handled-kind keep it, a miss drops it.
        assert!(
            bench
                .list_component_binaries(&ListComponentBinaries {
                    namespace: Some(PROBE_NAMESPACE.to_owned()),
                    handled_kind: None,
                    limit: None,
                    include_history: false,
                })
                .components
                .iter()
                .any(|e| e.hash == hash),
            "a matching namespace filter keeps the entry",
        );
        assert!(
            bench
                .list_component_binaries(&ListComponentBinaries {
                    namespace: None,
                    handled_kind: Some(Tick::ID),
                    limit: None,
                    include_history: false,
                })
                .components
                .iter()
                .any(|e| e.hash == hash),
            "a matching handled-kind filter keeps the entry",
        );
        assert!(
            !bench
                .list_component_binaries(&ListComponentBinaries {
                    namespace: Some("not_a_namespace".to_owned()),
                    handled_kind: None,
                    limit: None,
                    include_history: false,
                })
                .components
                .iter()
                .any(|e| e.hash == hash),
            "a non-matching namespace filter drops the entry",
        );

        // A second identical upload dedups to the same content hash.
        let again = match bench.upload_component(probe_path, None) {
            UploadComponentResult::Ok { hash, .. } => hash,
            UploadComponentResult::Err { error } => panic!("re-upload failed: {error}"),
        };
        assert_eq!(again, hash, "an identical re-upload dedups to the same hash");
        hash
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "infra test spawns same-process allocator threads to force contention"
    )]
    #[test]
    fn fleetbench_store_roots_are_unique_and_created_before_use() {
        const THREADS: usize = 8;

        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            handles.push(thread::spawn(allocate_store_root_for_test));
        }
        let roots: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("store-root allocator thread should not panic"))
            .collect();

        let unique: BTreeSet<_> = roots.iter().cloned().collect();
        assert_eq!(unique.len(), roots.len(), "each allocator call should return a unique root: {roots:?}");
        for root in &roots {
            assert!(root.is_dir(), "the allocator should create the root before returning it: {}", root.display());
        }
        for root in roots {
            fs::remove_dir_all(&root)
                .unwrap_or_else(|e| panic!("cleanup of store root {} failed ({e})", root.display()));
        }
    }

    #[test]
    fn fleetbench_component_listing_controls_cross_the_hub_boundary() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let probe_path = component_wasm_path("aether_test_fixtures_bundle");
        let mut bench = FleetBench::start();
        let named_hash = match bench.upload_component(probe_path.to_string_lossy().as_ref(), Some("probe")) {
            UploadComponentResult::Ok { hash, .. } => hash,
            UploadComponentResult::Err { error } => panic!("upload_component failed: {error}"),
        };

        // Append a valid, ignored custom section so the same manifest has a
        // distinct content hash for the unnamed history row.
        let mut historical_wasm = fs::read(&probe_path).expect("read probe wasm");
        historical_wasm.extend_from_slice(&[0, 3, 1, b'x', 0]);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos());
        let historical_path = env::temp_dir().join(format!("aether-fb-history-{}-{nanos}.wasm", process::id()));
        fs::write(&historical_path, historical_wasm).expect("write historical wasm fixture");
        let unnamed_hash = match bench.upload_component(historical_path.to_string_lossy().as_ref(), None) {
            UploadComponentResult::Ok { hash, name } => {
                assert!(name.is_none());
                hash
            }
            UploadComponentResult::Err { error } => panic!("uploading unnamed component history failed: {error}"),
        };

        let live = bench.list_component_binaries(&ListComponentBinaries::default());
        assert_eq!(live.total_matched, 1, "the default page contains only name-pointed registry rows");
        assert_eq!(live.components.len(), 1);
        assert_eq!(live.components[0].hash, named_hash);
        assert!(!live.components.iter().any(|entry| entry.hash == unnamed_hash));

        let history = bench.list_component_binaries(&ListComponentBinaries {
            include_history: true,
            ..ListComponentBinaries::default()
        });
        assert_eq!(history.total_matched, 2, "history opt-in includes the unnamed hash across RPC");
        assert!(history.components.iter().any(|entry| entry.hash == unnamed_hash));

        let zero = bench.list_component_binaries(&ListComponentBinaries {
            limit: Some(0),
            include_history: true,
            ..ListComponentBinaries::default()
        });
        assert!(zero.components.is_empty(), "an explicit zero limit survives the RPC boundary");
        assert_eq!(zero.total_matched, 2, "the pre-cap match count survives the RPC boundary");
        let _ = fs::remove_file(historical_path);
    }

    /// Upload the probe component by staged path, then assert the store
    /// ingested it content-addressed with a manifest read from the wasm,
    /// resolves + loads it by name / hash / handled-kind, dedups an
    /// identical re-upload, and replaces by hash. The replacement must
    /// initialize a fresh probe instance: its one-shot first-tick log must
    /// appear again after the pre-replace log cursor at the same lineage
    /// address, so a same-hash no-op cannot satisfy the test.
    #[test]
    fn fleetbench_uploads_resolves_loads_and_replaces_a_component() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let probe_path = component_wasm_path("aether_test_fixtures_bundle").to_string_lossy().into_owned();
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
                ComponentSelector { query: Some("probe".to_owned()), namespace: None, handled_kind: None }
            ),
            hash,
            "the name selector resolves to the probe hash",
        );
        assert_eq!(
            resolve_hash(
                &mut bench,
                ComponentSelector { query: Some(hash.clone()), namespace: None, handled_kind: None }
            ),
            hash,
            "the hash selector resolves to the probe hash",
        );
        assert_eq!(
            resolve_hash(&mut bench, ComponentSelector { query: None, namespace: None, handled_kind: Some(Tick::ID) }),
            hash,
            "the handled-kind attribute selector resolves to the probe hash",
        );

        let default_resolve = bench.resolve_component(ComponentSelector {
            query: Some("probe".to_owned()),
            namespace: None,
            handled_kind: None,
        });
        match default_resolve {
            ResolveComponentResult::Ok { config_kind, .. } => {
                assert!(config_kind.is_none(), "the default probe export has no typed config");
            }
            ResolveComponentResult::Err { error } => panic!("resolve failed: {error}"),
        }

        let typed_resolve = bench.resolve_component(ComponentSelector {
            query: Some("probe@test.probe_with_config".to_owned()),
            namespace: None,
            handled_kind: None,
        });
        match typed_resolve {
            ResolveComponentResult::Ok { config_kind, .. } => {
                let config_kind = config_kind.expect("typed-config export should carry a config descriptor");
                assert_eq!(config_kind.id, ProbeConfig::ID);
                assert_eq!(config_kind.name, ProbeConfig::NAME);
                let schema: SchemaType =
                    wire::from_bytes(&config_kind.schema_wire).expect("config schema descriptor should decode");
                assert_eq!(schema, ProbeConfig::SCHEMA);
            }
            ResolveComponentResult::Err { error } => panic!("typed resolve failed: {error}"),
        }

        // Fork a headless engine, load by selector (resolve-and-forward),
        // and assert it registers at the lineage address and answers
        // LogTail (it's live).
        let engine = bench.spawn_headless();
        let expected = probe_lineage_addr();
        let loaded = bench.load_by_selector(engine, "probe");
        assert_eq!(loaded.addr, expected, "load by selector registers at the lineage addr");
        assert!(
            bench.send(engine, &expected, &Tick).is_empty(),
            "a direct Tick used to observe the initial guest lifecycle should not reply",
        );
        let (first_tick_sequence, first_tick_cursor) =
            poll_probe_first_tick(&mut bench, engine, &expected, None, "initial load");

        // Replace the loaded component by hash (ADR-0022 in-place swap,
        // ADR-0116 exact-hash selector). The trampoline keeps its lineage
        // address, while the new probe instance resets its tick counter and
        // therefore emits the one-shot first-tick log again. A resolver or
        // trampoline optimization that treats the same hash as a no-op would
        // retain the old nonzero counter and fail this lifecycle oracle.
        let caps = bench.replace_by_selector(engine, loaded.mailbox_id, &hash);
        assert!(caps.handlers.iter().any(|h| h.id == Tick::ID), "the replaced probe still advertises its Tick handler");
        assert!(
            bench.send(engine, &expected, &Tick).is_empty(),
            "a direct Tick used to observe the replacement lifecycle should not reply",
        );
        let (replacement_tick_sequence, _) =
            poll_probe_first_tick(&mut bench, engine, &expected, Some(first_tick_cursor), "same-hash replacement");
        assert!(
            replacement_tick_sequence > first_tick_sequence,
            "the replacement's first-tick entry should be newer than the original (first={first_tick_sequence}, \
             replacement={replacement_tick_sequence})",
        );
        let registered = bench.list_components(engine);
        assert!(
            registered.iter().any(|addr| addr == &expected),
            "the replacement should remain registered at its original lineage address {expected}: {registered:?}",
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
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let probe_path = component_wasm_path("aether_test_fixtures_bundle").to_string_lossy().into_owned();
        let mut bench = FleetBench::start();

        let hash = match bench.upload_component(&probe_path, Some("probe")) {
            UploadComponentResult::Ok { hash, .. } => hash,
            UploadComponentResult::Err { error } => panic!("upload_component failed: {error}"),
        };

        // Pre-resolve the selector hub-local to the wasm bytes (the
        // aether-mcp boot-manifest pre-resolution hop), stage them to a
        // temp wasm file, and write a path-based boot manifest pointing at
        // it — the same shape aether-mcp's `stage_boot_manifest` produces.
        let wasm =
            match bench.resolve_component(ComponentSelector { query: Some(hash), namespace: None, handled_kind: None })
            {
                ResolveComponentResult::Ok { wasm, .. } => wasm,
                ResolveComponentResult::Err { error } => {
                    panic!("resolve for boot manifest failed: {error}")
                }
            };
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let staged_wasm = env::temp_dir().join(format!("aether-fb-boot-{}-{nanos}.wasm", process::id()));
        fs::write(&staged_wasm, &wasm).expect("stage the resolved boot wasm");
        let manifest_path = env::temp_dir().join(format!("aether-fb-manifest-{}-{nanos}.json", process::id()));
        // No explicit `name`: the trampoline registers at the
        // namespace-derived ADR-0099 lineage address, matching the load
        // path and `probe_lineage_addr()`.
        let manifest_json = serde_json::json!({
            "components": [{ "wasm": staged_wasm.to_string_lossy() }],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
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
        let registered = poll_until(|| bench.list_components(engine).iter().any(|n| n == &expected));
        assert!(registered, "the boot-manifest probe should come up and register at {expected}");

        // Best-effort: clean up the staged temp files.
        let _ = fs::remove_file(&staged_wasm);
        let _ = fs::remove_file(&manifest_path);
    }
}
