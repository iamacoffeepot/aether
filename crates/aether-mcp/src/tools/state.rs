use super::bytes::resolve_bytes_params;
use super::components::{ResolvedComponent, StagedBootManifest};
use super::ids::{mail_node_to_json, node_reversible_ids};
use super::{
    AWAIT_TIMEOUT_DEFAULT_MS, AsyncMutex, ComponentSelector, ComponentSpec, ENGINE_CAP, EngineId, EngineNames,
    INVENTORY_CAP, Kind, KindDescriptor, KindId, ListKinds, ListKindsResult, MailEnvelope, MailNodeJson, MailNodeWire,
    MailSpec, MailboxAddress, Manifest, ManifestResult, Mcp, McpError, Resolve, ResolveComponent,
    ResolveComponentResult, ResolveResult, SchemaType, Uuid, component_config_bytes, descriptors, engine_envelope,
    frame_size_aware_error, internal_msg, local_envelope, mailbox_id_from_path, max_frame_size, reject_zero_replicas,
    replica_base_name, replica_names, selector_with_explicit_export, tagged_id, validate_recipient_scope, wire,
};
use aether_data::canonical::kind_id_from_parts;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;

impl Mcp {
    /// Build one `MailSpec` into an `engine = Some` envelope and route
    /// it through the hub, awaiting the substrate's terminal settle and
    /// surfacing the correlated reply events (issue 1242). Returns the
    /// collected reply envelopes plus a `timed_out` flag — the await is
    /// bounded by [`AWAIT_TIMEOUT_DEFAULT_MS`] so a cap that never
    /// replies returns at the cap rather than hanging.
    pub(super) async fn deliver_one(&self, spec: MailSpec) -> anyhow::Result<(Vec<MailEnvelope>, bool)> {
        let envelope = self.build_mail_envelope(spec).await?;
        let timeout = Duration::from_millis(u64::from(AWAIT_TIMEOUT_DEFAULT_MS));
        self.session.call_collecting(envelope, timeout).await
    }

    /// [`Self::deliver_one`]'s fire-and-forget twin: build the envelope
    /// and write the `Call` without awaiting any reply (issue 1242).
    pub(super) async fn deliver_one_fire(&self, spec: MailSpec) -> anyhow::Result<()> {
        let envelope = self.build_mail_envelope(spec).await?;
        self.session.fire(envelope).await
    }

    /// Resolve a component registry selector hub-local to its wasm bytes +
    /// `@actor` export (ADR-0116). aether-mcp issues a `ResolveComponent` to
    /// the `aether.engine` cap (no engine route — the store is hub-level),
    /// which matches the selector to a single component and replies with the
    /// wasm bytes from its store; aether-mcp then forwards those bytes to
    /// the target substrate's `aether.component` mailbox. Shared by
    /// `load_component`, `replace_component`, and the boot-manifest
    /// pre-resolution, so the load seam stays path-free. An `Err` reply (no
    /// match, or an attribute query matching more than one component) is a
    /// clean tool error.
    pub(super) async fn resolve_component(&self, selector: &str) -> Result<ResolvedComponent, McpError> {
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &ResolveComponent {
                    selector: ComponentSelector {
                        query: Some(selector.to_owned()),
                        namespace: None,
                        handled_kind: None,
                    },
                },
            ))
            .await
            .map_err(|e| frame_size_aware_error(&format!("resolve_component {selector:?}"), e))?;
        match ResolveComponentResult::decode_from_bytes(&reply.payload) {
            Some(ResolveComponentResult::Ok { wasm, export, manifest, config_kind, .. }) => {
                // ADR-0138: the bare-load default is the manifest's opted-in
                // `default_entry` (the single-actor namespace or the
                // `export!(default = …)` designation), NOT "the first actor
                // by list position". A defaultless multi-actor module
                // reports `None`, so replica-name derivation falls through
                // to `name` / `export`.
                let default_namespace = manifest.default_entry;
                Ok(ResolvedComponent { wasm, export, default_namespace, config_kind })
            }
            Some(ResolveComponentResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable ResolveComponentResult")),
        }
    }

    /// Pre-resolve a `spawn_substrate` boot list against the component
    /// registry (ADR-0116) and stage it as a temp boot-manifest JSON the
    /// hub injects as `AETHER_BOOT_MANIFEST` (issue 1776). For each spec
    /// aether-mcp resolves the selector hub-local to its wasm bytes, writes
    /// the bytes to a per-process-unique temp `.wasm`, and points the
    /// manifest entry's `wasm` at that staged path — so the substrate boot
    /// autoload path stays path-based, now fed by the registry rather than
    /// host build paths. A `module@actor` selector's `@actor` half
    /// populates the entry's `export` unless the spec set one explicitly.
    /// Returns the staged paths so the caller cleans them all up once the
    /// substrate has read them at boot; a staging failure removes whatever
    /// it already wrote before surfacing the error.
    pub(super) async fn stage_boot_manifest(
        &self,
        components: &[ComponentSpec],
    ) -> Result<StagedBootManifest, McpError> {
        let mut wasm_paths: Vec<PathBuf> = Vec::with_capacity(components.len());
        let mut config_paths: Vec<PathBuf> = Vec::new();
        match self.stage_boot_files(components, &mut wasm_paths, &mut config_paths).await {
            Ok((manifest_path, expected_names)) => {
                Ok(StagedBootManifest { manifest_path, wasm_paths, config_paths, expected_names })
            }
            Err(e) => {
                // No StagedBootManifest reaches the caller on this path, so
                // nothing else will ever clean these up — best-effort remove
                // every file staged before the failure.
                for path in wasm_paths.iter().chain(config_paths.iter()) {
                    let _ = fs::remove_file(path).await;
                }
                Err(e)
            }
        }
    }

    /// The fallible staging body of [`Self::stage_boot_manifest`]: pushes
    /// each staged path before attempting its `fs::write`, so the caller's
    /// cleanup covers a partially-written file as well as later failures.
    async fn stage_boot_files(
        &self,
        components: &[ComponentSpec],
        wasm_paths: &mut Vec<PathBuf>,
        config_paths: &mut Vec<PathBuf>,
    ) -> Result<(PathBuf, Vec<String>), McpError> {
        use std::env;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let mut entries: Vec<serde_json::Value> = Vec::with_capacity(components.len());
        let mut expected_names: Vec<String> = Vec::with_capacity(components.len());
        for spec in components {
            reject_zero_replicas(spec.replicas, &spec.selector)?;
            let resolve_selector = selector_with_explicit_export(&spec.selector, spec.export.as_deref());
            let resolved = self.resolve_component(&resolve_selector).await?;
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let wasm_path = env::temp_dir().join(format!("aether-boot-wasm-{}-{seq}.wasm", process::id()));
            wasm_paths.push(wasm_path.clone());
            fs::write(&wasm_path, &resolved.wasm)
                .await
                .map_err(|e| internal_msg(&format!("staging boot wasm for selector {resolve_selector:?}: {e}")))?;
            let mut entry = serde_json::json!({ "wasm": wasm_path.to_string_lossy() });
            if let Some(name) = &spec.name {
                entry["name"] = serde_json::json!(name);
            }
            if let Some(config) = component_config_bytes(
                resolved.config_kind.as_ref(),
                spec.config.clone(),
                spec.config_path.as_deref(),
                &format!("spawn_substrate component {resolve_selector:?}"),
            )
            .await?
            {
                let seq = SEQ.fetch_add(1, Ordering::Relaxed);
                let config_path = env::temp_dir().join(format!("aether-boot-config-{}-{seq}.bin", process::id()));
                config_paths.push(config_path.clone());
                fs::write(&config_path, config).await.map_err(|e| {
                    internal_msg(&format!("staging boot config for selector {resolve_selector:?}: {e}"))
                })?;
                entry["config"] = serde_json::json!(config_path.to_string_lossy());
            }
            // An explicit `export` wins over the selector's `@actor` half.
            let export = spec.export.clone().or_else(|| resolved.export.clone());
            if let Some(ref e) = export {
                entry["export"] = serde_json::json!(e);
            }
            // issue 2626: pass `replicas` into the staged manifest entry
            // verbatim — `autoload::expand_replicas` fans it out at read time.
            if let Some(replicas) = spec.replicas {
                entry["replicas"] = serde_json::json!(replicas);
            }
            // Derive the expected registered name(s) in the same precedence
            // order the engine applies: caller-supplied name > export
            // namespace > default actor namespace. Fail loud if none is
            // determinable: a spawn that can't name what it's waiting for is
            // a bug to surface at stage time.
            let ns = replica_base_name(spec.name.as_deref(), export.as_deref(), resolved.default_namespace.as_deref())
                .ok_or_else(|| {
                    internal_msg(&format!(
                        "component {:?}: cannot determine expected registered name \
                     (no `name`, `export`, or default actor namespace in the wasm manifest); \
                     set `name` or `export` on the ComponentSpec",
                        spec.selector,
                    ))
                })?;
            match spec.replicas {
                Some(replicas) => expected_names.extend(
                    replica_names(&ns, replicas)
                        .into_iter()
                        .map(|name| format!("aether.component/aether.embedded:{name}")),
                ),
                None => expected_names.push(format!("aether.component/aether.embedded:{ns}")),
            }
            entries.push(entry);
        }

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let manifest_path = env::temp_dir().join(format!("aether-boot-manifest-{}-{seq}.json", process::id()));
        let bytes = serde_json::to_vec(&serde_json::json!({ "components": entries }))
            .map_err(|e| internal_msg(&format!("encoding boot manifest: {e}")))?;
        if let Err(e) = fs::write(&manifest_path, bytes).await {
            // A failed write can still create the file — remove it here, since
            // the manifest path never reaches the caller's cleanup vecs.
            let _ = fs::remove_file(&manifest_path).await;
            return Err(internal_msg(&format!("staging boot manifest: {e}")));
        }
        Ok((manifest_path, expected_names))
    }

    /// Resolve a `MailSpec` against the per-engine merged kind view
    /// (static prefill + cached `ListKinds` reply, ADR-0091) and build
    /// the `engine = Some` wire envelope — the shared front half of
    /// [`Self::deliver_one`] / [`Self::deliver_one_fire`]. The resolve +
    /// encode runs through [`Self::resolve_and_encode`], so an unknown-kind
    /// miss *or* a field-mismatch encode failure (a kind widened in place,
    /// ADR-0091 §3) triggers one `aether.inventory.kinds` refresh-and-retry
    /// before the error surfaces.
    pub(super) async fn build_mail_envelope(&self, spec: MailSpec) -> anyhow::Result<MailEnvelope> {
        // ADR-0098/0099 wire boundary: `recipient_name` is user-controlled
        // and folds to a registry key, so cap its scope depth / byte size
        // before it reaches `mailbox_id_from_path` (the fold itself stays
        // infallible for static callers). A breach fails this mail item.
        validate_recipient_scope(&spec.recipient_name)?;
        let engine = EngineId(
            Uuid::parse_str(&spec.engine_id).map_err(|e| anyhow::anyhow!("engine_id is not a valid UUID: {e}"))?,
        );
        let params = spec.params.unwrap_or(serde_json::Value::Null);
        let (desc, payload) = self.resolve_and_encode(engine, &spec.kind_name, params).await?;
        Ok(MailEnvelope {
            to: MailboxAddress {
                engine: Some(engine),
                // ADR-0099 §4: resolve the recipient by the parse → fold,
                // so a `/`-rendered hosted / nested actor name
                // (`aether.component/aether.component/aether.embedded:aether.camera`)
                // reaches its lineage-folded id. A root-cap name is a
                // single segment and folds to the same id `hash(name)`
                // gives.
                mailbox: mailbox_id_from_path(&spec.recipient_name),
            },
            from: None,
            kind: KindId(kind_id_from_parts(&desc.name, &desc.schema)),
            correlation_id: None,
            payload,
        })
    }

    /// Resolve `params` against the engine's live schema for `kind_name`
    /// and schema-encode them, refreshing the per-engine kind cache once
    /// on a staleness signal (ADR-0091 §3). The single shared encode path
    /// behind `send_mail` ([`Self::build_mail_envelope`]),
    /// `send_mail_traced` ([`Self::encode_traced_bundle`]), and
    /// `capture_frame` ([`Self::encode_capture_bundle`]).
    ///
    /// Two staleness triggers refresh the cache from
    /// `aether.inventory.kinds`, both bounded to exactly one refresh
    /// round-trip:
    ///
    /// - **Unknown-kind miss** — handled one layer down by
    ///   [`Self::lookup_descriptor`]: the name isn't in the cache, so the
    ///   refresh runs before the descriptor is returned.
    /// - **Field-mismatch encode failure** — the name *is* cached but its
    ///   schema is stale (a kind widened in place, e.g. `aether.mouse_button`
    ///   gaining a field). The name resolves, so `lookup_descriptor` returns
    ///   a hit with the narrow schema, and `encode_schema` rejects the new
    ///   field. That encode failure — and *only* an encode failure, not a
    ///   `resolve_bytes_params` blob-IO error — triggers the same refresh,
    ///   re-resolves + re-encodes against the fresh schema once, and returns.
    ///
    /// The retry re-runs `resolve_bytes_params` (not just `encode_schema`)
    /// so a widening that adds a `Bytes` field descends into it and resolves
    /// its `$`-sigil embed against the fresh schema. It is bounded to one
    /// refresh + one re-encode and never re-enters the refresh branch, so
    /// there is no retry loop by construction. If the fresh vocab still
    /// rejects the params the retry's error surfaces; if the name vanished
    /// from the fresh vocab (a removal, not a widening) the original encode
    /// error surfaces.
    pub(super) async fn resolve_and_encode(
        &self,
        engine: EngineId,
        kind_name: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<(KindDescriptor, Vec<u8>)> {
        let desc = self.lookup_descriptor(engine, kind_name).await?;
        let resolved = resolve_bytes_params(params.clone(), &desc.schema, max_frame_size())
            .await
            .map_err(|e| anyhow::anyhow!("resolving blob params: {e}"))?;
        match aether_codec::encode_schema(&resolved, &desc.schema) {
            Ok(payload) => Ok((desc, payload)),
            Err(encode_err) => {
                // The cached schema may be stale from an in-place kind
                // widening (ADR-0091 §3). Unlike the miss path, we don't
                // re-check cache presence after taking the guard — a
                // stale-schema name *is* present, so a presence re-check
                // would wrongly short-circuit before refreshing. N
                // concurrent field-mismatch retries can each issue a
                // refresh RPC; an accepted cold-path cost (encode failures
                // are mistakes, not hot traffic), still one refresh per
                // retry, no loop.
                let guard = self.refresh_guard(engine);
                let _refresh = guard.lock().await;
                self.refresh_engine_kinds(engine).await;
                let Some(fresh) = self.cache_lookup(engine, kind_name) else {
                    // The name vanished from the fresh vocab (a removal,
                    // not a widening) — surface the original encode error.
                    return Err(anyhow::anyhow!("param encode failed: {encode_err}"));
                };
                let resolved = resolve_bytes_params(params, &fresh.schema, max_frame_size())
                    .await
                    .map_err(|e| anyhow::anyhow!("resolving blob params: {e}"))?;
                let payload = aether_codec::encode_schema(&resolved, &fresh.schema)
                    .map_err(|e| anyhow::anyhow!("param encode failed: {e}"))?;
                Ok((fresh, payload))
            }
        }
    }

    /// Ensure `engine`'s reverse-name map is built (ADR-0088 §8). On the
    /// first need for an engine, fetch `aether.inventory.manifest` and
    /// fold it into an [`EngineNames`]; on any subsequent call the cached
    /// map is reused. An engine that doesn't answer the manifest (older
    /// build, headless without the cap, transient error) gets an empty
    /// map cached — every lookup then falls back to the hex tag rather
    /// than erroring the tool or re-fetching on every render.
    pub(super) async fn ensure_names(&self, engine: EngineId) {
        if self.names.lock().expect("reverse-name cache mutex is never poisoned").contains_key(&engine) {
            return;
        }
        // Fetch outside the lock — the await must not hold a std Mutex.
        // No / undecodable reply caches an empty map so we fall back to
        // hex for this engine without re-querying every render.
        let manifest = self
            .session
            .call_one(engine_envelope(engine, INVENTORY_CAP, &Manifest {}))
            .await
            .ok()
            .and_then(|reply| ManifestResult::decode_from_bytes(&reply.payload))
            .unwrap_or_else(|| ManifestResult { names: Vec::new(), templates: Vec::new() });
        let mut cache = self.names.lock().expect("reverse-name cache mutex is never poisoned");
        // A concurrent session may have populated it while we awaited —
        // first writer wins, both maps are equivalent.
        cache.entry(engine).or_insert_with(|| EngineNames::from_manifest(&manifest));
    }

    /// ADR-0091 §3 lookup → miss → refresh → retry → error flow. Look
    /// up `kind_name` in the engine's encode cache; on a miss, fetch
    /// the substrate's authoritative vocabulary via
    /// `aether.inventory.kinds` and retry. The per-engine refresh is
    /// collapsed under an async mutex so two concurrent misses on
    /// different unknown names trigger one RPC, not two.
    ///
    /// Returns the matched descriptor; errors with `unknown kind: …`
    /// after one refresh round-trip if the engine doesn't recognise
    /// the name (a typoed kind, or a kind belonging to a component
    /// that hasn't been loaded yet — distinguishable by the error
    /// type at the substrate's later dispatch attempt).
    ///
    /// This covers only the unknown-**name** trigger. A cached name whose
    /// *schema* has gone stale (a kind widened in place) still resolves to
    /// a hit here; that field-mismatch staleness is caught one layer up in
    /// [`Self::resolve_and_encode`], which refreshes on the encode failure.
    pub(super) async fn lookup_descriptor(&self, engine: EngineId, kind_name: &str) -> anyhow::Result<KindDescriptor> {
        // Fast path: hit on the cache as it stands. `prefill_engine`
        // populates the static `descriptors::all()` baseline on first
        // touch so the very first send for a substrate-vocab kind
        // doesn't trip a refresh.
        self.prefill_engine(engine);
        if let Some(desc) = self.cache_lookup(engine, kind_name) {
            return Ok(desc);
        }

        // Miss: take the per-engine refresh mutex, then re-check (a
        // concurrent waiter may have just refreshed) and refresh
        // ourselves if still missing.
        let guard = self.refresh_guard(engine);
        let _refresh = guard.lock().await;
        if let Some(desc) = self.cache_lookup(engine, kind_name) {
            return Ok(desc);
        }
        self.refresh_engine_kinds(engine).await;
        self.cache_lookup(engine, kind_name).ok_or_else(|| anyhow::anyhow!("unknown kind: {kind_name}"))
    }

    /// Seed `engine`'s cache from the substrate's static vocabulary
    /// (`descriptors::all`) the first time the harness touches it. The
    /// static set is process-global so a second engine with the same
    /// build sees the same prefill, but the cache is keyed per-engine
    /// because component-defined kinds aren't shared across engines.
    #[allow(clippy::significant_drop_tightening)] // tight scope already
    pub(super) fn prefill_engine(&self, engine: EngineId) {
        let mut cache = self.kinds.descriptors.lock().expect("kinds-cache mutex is never poisoned");
        cache.entry(engine).or_insert_with(|| descriptors::all().into_iter().map(|d| (d.name.clone(), d)).collect());
    }

    /// Synchronous cache hit/miss check — no await, holds the std
    /// `Mutex` only across the cloning lookup.
    pub(super) fn cache_lookup(&self, engine: EngineId, kind_name: &str) -> Option<KindDescriptor> {
        let cache = self.kinds.descriptors.lock().expect("kinds-cache mutex is never poisoned");
        cache.get(&engine).and_then(|m| m.get(kind_name).cloned())
    }

    /// Fetch-or-create the per-engine refresh mutex. The mutex is
    /// wrapped in an `Arc` so we can drop the cache lock before
    /// awaiting; the only writers to `refresh_guards` are this
    /// function itself, so it's a small concurrent-insert with no
    /// upstream contention.
    pub(super) fn refresh_guard(&self, engine: EngineId) -> Arc<AsyncMutex<()>> {
        let mut guards = self.kinds.refresh_guards.lock().expect("kinds-cache refresh-guards mutex is never poisoned");
        guards.entry(engine).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
    }

    /// Issue `ListKinds` against the engine's `aether.inventory`
    /// mailbox and replace this engine's cache with the reply (folded
    /// over the static prefill — a component's own kinds layer on top
    /// of the substrate baseline, neither side wins exclusively).
    /// A failed RPC / undecodable reply leaves the cache untouched so
    /// the caller's retry surfaces the original "unknown kind" miss
    /// rather than a transient RPC error.
    pub(super) async fn refresh_engine_kinds(&self, engine: EngineId) {
        let Some(ListKindsResult { kinds }) = self
            .session
            .call_one(engine_envelope(engine, INVENTORY_CAP, &ListKinds {}))
            .await
            .ok()
            .and_then(|reply| ListKindsResult::decode_from_bytes(&reply.payload))
        else {
            return;
        };

        // Decode each `schema_wire` back into a `SchemaType` via
        // `wire::from_bytes`; an entry whose schema fails to decode is
        // dropped (the substrate's wire form is canonical, so a decode
        // failure is a substrate / aether-data version mismatch —
        // better to skip the entry than panic the tool call).
        let fresh: Vec<KindDescriptor> = kinds
            .into_iter()
            .filter_map(|wire| {
                let schema = wire::from_bytes::<SchemaType>(&wire.schema_wire).ok()?;
                Some(KindDescriptor { name: wire.name, schema })
            })
            .collect();

        // Hold the cache lock only for the merge; the await above is
        // already complete, so the `MutexGuard`'s significant `Drop`
        // doesn't span any await point.
        self.merge_into_engine_cache(engine, fresh);
    }

    /// Merge `fresh` into `engine`'s cache map, replacing any prior
    /// entries with the same name. Factored out of `refresh_engine_kinds`
    /// so the cache lock is acquired in a tight scope — no other state
    /// hangs off the same critical section, and no await crosses the
    /// guard.
    #[allow(clippy::significant_drop_tightening)] // tight scope already
    pub(super) fn merge_into_engine_cache(&self, engine: EngineId, fresh: Vec<KindDescriptor>) {
        let mut cache = self.kinds.descriptors.lock().expect("kinds-cache mutex is never poisoned");
        let map = cache.entry(engine).or_default();
        for desc in fresh {
            map.insert(desc.name.clone(), desc);
        }
    }

    /// Snapshot the per-engine kind descriptor map for reply decoding
    /// (issue 1804). Returns a clone of `engine`'s `name → descriptor`
    /// map, or an empty map when the cache hasn't been seeded for that
    /// engine yet. The engine cache is prefilled from the static substrate
    /// vocabulary on the first `build_mail_envelope` call (`prefill_engine`),
    /// so an empty snapshot only arises on paths that never encoded a kind
    /// for this engine (e.g. a broken `engine_id` before `deliver_one` errored).
    pub(super) fn snapshot_engine_kinds(&self, engine: EngineId) -> HashMap<String, KindDescriptor> {
        self.kinds
            .descriptors
            .lock()
            .expect("kinds-cache mutex is never poisoned")
            .get(&engine)
            .cloned()
            .unwrap_or_default()
    }

    /// Batch-resolve `ids` that the engine's static map missed via
    /// `aether.inventory.resolve` and fold the answers (positive *and*
    /// negative) into the per-engine dynamic cache. A no-op when `ids` is
    /// empty or the resolve call fails — the unresolved ids then render
    /// as hex tags. `ids` are raw `u64`s; the resolve wire takes tagged
    /// strings, so each is encoded for the request and decoded back for
    /// the cache key.
    pub(super) async fn resolve_dynamic(&self, engine: EngineId, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        let tagged: Vec<String> = ids.iter().filter_map(|id| tagged_id::encode(*id)).collect();
        if tagged.is_empty() {
            return;
        }
        let Some(ResolveResult { resolved }) = self
            .session
            .call_one(engine_envelope(engine, INVENTORY_CAP, &Resolve { ids: tagged }))
            .await
            .ok()
            .and_then(|reply| ResolveResult::decode_from_bytes(&reply.payload))
        else {
            return;
        };
        let mut cache = self.names.lock().expect("reverse-name cache mutex is never poisoned");
        if let Some(names) = cache.get_mut(&engine) {
            for entry in resolved {
                if let Ok(id) = tagged_id::decode(&entry.id) {
                    names.cache_resolved(id, entry.name);
                }
            }
        }
    }

    /// Reverse-render every id in a settled trace tree to a real name
    /// (ADR-0088 §8). Builds the engine's reverse map if needed, collects
    /// the ids that the static map misses, resolves them in one batched
    /// `aether.inventory.resolve` query, then renders each `MailNodeWire`
    /// through the now-populated map — falling back to the ADR-0064 hex
    /// tag for any id that resolves to nothing. `Handle` ids stay hex
    /// (they never enter the reverse map).
    pub(super) async fn render_mail_nodes(&self, engine: EngineId, nodes: Vec<MailNodeWire>) -> Vec<MailNodeJson> {
        self.ensure_names(engine).await;

        // Collect the mailbox / kind / thread ids that the static map
        // misses, so one batched resolve covers the whole tree.
        let mut misses: Vec<u64> = Vec::new();
        {
            let cache = self.names.lock().expect("reverse-name cache mutex is never poisoned");
            if let Some(names) = cache.get(&engine) {
                for node in &nodes {
                    for id in node_reversible_ids(node) {
                        if names.needs_resolve(id) {
                            misses.push(id);
                        }
                    }
                }
            }
        }
        misses.sort_unstable();
        misses.dedup();
        self.resolve_dynamic(engine, &misses).await;

        let cache = self.names.lock().expect("reverse-name cache mutex is never poisoned");
        match cache.get(&engine) {
            Some(names) => nodes.into_iter().map(|node| mail_node_to_json(node, Some(names))).collect(),
            // No map for this engine (shouldn't happen post-ensure, but
            // be defensive): render every id as a hex tag.
            None => nodes.into_iter().map(|n| mail_node_to_json(n, None)).collect(),
        }
    }
}
