//! Binary / component artifact resolution + ingestion for the engines
//! cap (ADR-0115 / ADR-0116). The content-addressed store seams the
//! handlers delegate to: ingest an uploaded binary / component, resolve
//! a [`BinarySelector`] / `ComponentSelector` to stored bytes, and
//! realize stored bytes to an executable temp file for fork+exec.
//! Native-only (forks `--describe`, reads / copies files).

use crate::store::{
    ArtifactKind, ArtifactStore, Selector, StoredArtifact, StoredManifest, component_manifest, config_descriptor,
};
use aether_kinds::{
    BinaryManifest, BinarySelector, ComponentSelector, ListComponentBinaries, ListEngineBinaries,
    ResolveComponentResult,
};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// The chassis a `default` selector (an empty [`BinarySelector::query`]
/// with no attribute filters) resolves to (ADR-0115): `headless` has no
/// window and runs on any host, so a bare spawn is self-sufficient.
const DEFAULT_CHASSIS: &str = "headless";

/// Fork `binary_path --describe` and parse the JSON manifest it prints
/// (ADR-0115, issue 1953). The one-time capture of what a binary *is* —
/// its chassis kind, linked caps, config surface, and build provenance —
/// without the hub linking the chassis crate. `stdin` is nulled so a
/// describe can't block on input.
///
/// The parsed manifest is held to strict shape conformance
/// ([`validate_manifest`]) before it can enter the store (ADR-0162 §
/// consume side, #3936): unstorable = unspawnable, so a binary that
/// skipped or diverged from the describe contract is rejected here rather
/// than trusted to author diligence. The rejection names the specific
/// malformation — the `--describe` output surfaces to an agent through the
/// `upload_binary` MCP error, so it must say what to fix.
fn describe_binary(binary_path: &str) -> Result<BinaryManifest, String> {
    let output = Command::new(binary_path)
        .arg("--describe")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("forking {binary_path:?} --describe: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{binary_path:?} --describe exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    // Parse twice off the same bytes: once to a raw `Value` so presence of
    // a `#[serde(default)]` field can be told apart from its default
    // (`env_keys` / `argv_flags` — a permissive typed parse alone can't
    // distinguish an absent field from an empty one, ADR-0162 read
    // policy), and once to the typed manifest the store holds. The raw
    // parse also decides the pre-ADR-0162 rejection message.
    let raw: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parsing {binary_path:?} --describe manifest JSON: {e}"))?;
    let manifest: BinaryManifest = serde_json::from_value(raw.clone())
        .map_err(|e| format!("parsing {binary_path:?} --describe manifest JSON: {e}"))?;
    validate_manifest(&manifest, &raw)
        .map_err(|why| format!("{binary_path:?} --describe manifest is nonconforming: {why}"))?;
    Ok(manifest)
}

/// Hold a parsed `--describe` manifest to strict shape conformance before
/// it enters the store (ADR-0162 consume side, #3936). Every field the
/// produce side ([`describe_manifest`](aether_substrate)) structurally
/// guarantees must be present and non-empty:
///
/// - `chassis` — the profile string is always `Chassis::PROFILE`.
/// - `caps` — every chassis composes at least the RPC server (ADR-0155
///   §3), so the claim roster is never empty.
/// - `git_sha` / `profile` / `target` — the bundle's `build.rs` bakes
///   `"unknown"` rather than an empty string when a fact is unavailable
///   (built outside a git checkout), so `"unknown"` is a *conforming*
///   value; only a truly empty string is a divergence.
/// - `env_keys` / `argv_flags` — the config surface every chassis
///   composes (ADR-0162): the RPC server alone contributes
///   `AETHER_RPC_PORT` / `--rpc-port`, so a conforming manifest carries a
///   non-empty set of each. These fields are `#[serde(default)]` on the
///   shared kind (an old stored sidecar reads back with empty sets rather
///   than failing the store, #3943), so *absent from the JSON* is checked
///   on the raw `Value` and rejected with a distinct "predates the
///   ADR-0162 config surface" message — the new fields joining the
///   required shape is the point of this gate: a binary built before the
///   config surface existed is unspawnable and must not enter the store.
///
/// This strictness applies to *new ingestion only*. The stored-manifest
/// read path stays permissive (ADR-0162 / #3943): entries already in the
/// store under old hashes keep parsing with defaulted fields.
///
/// Unknown-field policy: **tolerated**. The typed parse does not set
/// `deny_unknown_fields`, and this validator checks only for the presence
/// and shape of the fields it requires, ignoring any extra. A *newer*
/// binary that reports additional manifest fields must stay uploadable to
/// an *older* hub — rejecting unknown fields would make every hub a hard
/// floor on binary vintage, the opposite of the forward-compatible store.
fn validate_manifest(manifest: &BinaryManifest, raw: &serde_json::Value) -> Result<(), String> {
    for (field, value) in [
        ("chassis", manifest.chassis.as_str()),
        ("git_sha", manifest.git_sha.as_str()),
        ("profile", manifest.profile.as_str()),
        ("target", manifest.target.as_str()),
    ] {
        if value.is_empty() {
            return Err(format!("required field {field:?} is empty"));
        }
    }
    if manifest.caps.is_empty() {
        return Err("required field \"caps\" is empty — every chassis composes at least the RPC server (ADR-0155 §3)"
            .to_owned());
    }
    // `env_keys` / `argv_flags` are `#[serde(default)]`, so an absent field
    // decodes to an empty set indistinguishably from an empty one in the
    // JSON. Check presence on the raw value first, naming the pre-ADR-0162
    // vintage precisely, then require non-empty on the decoded set.
    for (field, decoded) in [("env_keys", &manifest.env_keys), ("argv_flags", &manifest.argv_flags)] {
        if raw.get(field).is_none() {
            return Err(format!(
                "manifest omits {field:?}: the binary predates the ADR-0162 config surface and cannot be addressed — rebuild it against a chassis-main that self-reports its config surface",
            ));
        }
        if decoded.is_empty() {
            return Err(format!(
                "required field {field:?} is empty — every chassis composes a config surface (the RPC server alone contributes AETHER_RPC_PORT / --rpc-port, ADR-0162)",
            ));
        }
    }
    Ok(())
}

/// Ingest the binary at `path` into `store` content-addressed,
/// capturing its manifest via a one-time `<path> --describe` fork
/// (ADR-0115, issue 1953). Shared by the `on_upload_binary` handler and
/// the [`bootstrap_ingest`] boot path. Returns the stored content hash,
/// or a human-readable error for an unreadable path or a `--describe`
/// that failed / yielded no parseable manifest. Idempotent — identical
/// bytes dedup to the same hash.
pub fn ingest_binary(store: &mut ArtifactStore, path: &str, name: Option<String>) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("reading binary path {path:?}: {e}"))?;
    let manifest = describe_binary(path)?;
    Ok(store.upload(&bytes, ArtifactKind::Binary, StoredManifest::Binary(manifest), name))
}

/// Bootstrap-ingest each chassis bin in `paths` into `store`, naming
/// each by its file stem so a `default` / `name` selector resolves in a
/// fresh or `restart-hub`'d hub (ADR-0115, issue 1954). The list rides
/// `FleetConfig`'s `binary_bootstrap` field (its `AETHER_BINARY_BOOTSTRAP`
/// env layer, ADR-0090). A path that can't be read or `--describe`d is
/// logged and skipped — a bad bootstrap entry must not fail hub boot.
/// Idempotent via content dedup.
pub fn bootstrap_ingest(store: &mut ArtifactStore, paths: &HashSet<String>) {
    for path in paths {
        let name = Path::new(path).file_stem().and_then(|stem| stem.to_str()).map(str::to_owned);
        match ingest_binary(store, path, name) {
            Ok(hash) => tracing::info!(
                target: "aether_substrate::fleet_server",
                path = path.as_str(),
                hash = %hash,
                "binary bootstrap: ingested a chassis bin",
            ),
            Err(error) => tracing::warn!(
                target: "aether_substrate::fleet_server",
                path = path.as_str(),
                error = %error,
                "binary bootstrap: skipping a bin that failed to ingest",
            ),
        }
    }
}

/// Ingest the component wasm at `path` into `store` content-addressed,
/// reading its manifest straight from the wasm (ADR-0116, issue 1956) —
/// no execution step. Returns the stored content hash, or a
/// human-readable error for an unreadable path or an unparseable wasm.
/// Idempotent — identical bytes dedup to the same hash.
pub fn ingest_component(store: &mut ArtifactStore, path: &str, name: Option<String>) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("reading component path {path:?}: {e}"))?;
    let manifest = component_manifest(&bytes).map_err(|e| format!("reading component manifest from {path:?}: {e}"))?;
    Ok(store.upload(&bytes, ArtifactKind::Component, StoredManifest::Component(manifest), name))
}

/// Resolve a [`ComponentSelector`] against `store` to its wasm bytes +
/// manifest (ADR-0116, issue 1956). Resolution order mirrors the binary
/// selector: an exact `query` token wins first
/// (`hash` > `module@actor` > `name@version` (latest in v1) > `name`);
/// absent a token, the `namespace` / `handled_kind` attribute query
/// resolves, where a query
/// matching more than one component is a clean ambiguity error (never a
/// silent pick). A `module@actor` token's `@actor` part populates the
/// reply `export` so the forwarded `LoadComponent` instantiates that
/// actor type (ADR-0096). Returns `Err` for no match / ambiguity.
pub fn resolve_component(store: &mut ArtifactStore, selector: &ComponentSelector) -> ResolveComponentResult {
    // An exact token, with the `@actor` half (if any) split off as the
    // export selector forwarded to the substrate.
    if let Some(token) = selector.query.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        return resolve_component_token(store, token);
    }
    // No exact token: a namespace / handled-kind attribute query. A
    // match-more-than-one is a clean ambiguity error.
    let mut matches = store.matching_components(&ListComponentBinaries {
        namespace: selector.namespace.clone(),
        handled_kind: selector.handled_kind,
        limit: None,
        include_history: false,
    });
    match matches.len() {
        0 => ResolveComponentResult::Err {
            error: format!(
                "no stored component matches the attribute query (namespace = {:?}, handled_kind = {:?})",
                selector.namespace, selector.handled_kind,
            ),
        },
        1 => {
            let hash = matches.remove(0).hash;
            stored_component_reply(store, &hash, None)
        }
        n => ResolveComponentResult::Err {
            error: format!(
                "the attribute query (namespace = {:?}, handled_kind = {:?}) matches {n} components — narrow it to a single component (by hash or name)",
                selector.namespace, selector.handled_kind,
            ),
        },
    }
}

/// Resolve an exact component selector token to a [`ResolveComponentResult`]
/// (ADR-0116). A `module@actor` token splits into the `module`
/// hash/name and the `@actor` export selector; a `name@version` token
/// is treated as `name` (latest) — v1 keeps no per-name version index;
/// a bare token resolves as a hash first, then a name.
fn resolve_component_token(store: &mut ArtifactStore, token: &str) -> ResolveComponentResult {
    // `module@actor` (ADR-0096) takes precedence: the `@actor` half is a
    // component `Addressable::NAMESPACE`, distinct from a binary `name@version`
    // build id. Resolve the module half (hash, then name), forward the
    // actor half as `export`.
    if let Some((module, actor)) = token.split_once('@') {
        // A hash never contains `@`, so the module half resolves as a
        // hash first, then a name (latest). The actor half is the export.
        if store.contains(module) {
            return stored_component_reply(store, module, Some(actor.to_owned()));
        }
        if let Some(found) = store.get(&Selector::Name(module.to_owned())) {
            return stored_component_reply(store, &found.hash, Some(actor.to_owned()));
        }
        return ResolveComponentResult::Err { error: format!("no stored component matches the selector {token:?}") };
    }
    // A bare token: an exact hash wins, else a name (latest).
    if store.contains(token) {
        return stored_component_reply(store, token, None);
    }
    if let Some(found) = store.get(&Selector::Name(token.to_owned())) {
        return stored_component_reply(store, &found.hash, None);
    }
    ResolveComponentResult::Err { error: format!("no stored component matches the selector {token:?}") }
}

/// Read the stored component `hash`'s wasm bytes + manifest off disk and
/// build a `ResolveComponentResult::Ok` (ADR-0116). `export` threads a
/// `module@actor` selector's actor half through to the forwarded
/// `LoadComponent.export`. An entry that isn't a component (a binary
/// hash) or whose bytes can't be read is a clean `Err`.
fn stored_component_reply(store: &mut ArtifactStore, hash: &str, export: Option<String>) -> ResolveComponentResult {
    let Some(found) = store.get(&Selector::Hash(hash.to_owned())) else {
        return ResolveComponentResult::Err { error: format!("no stored artifact has hash {hash:?}") };
    };
    let Some(manifest) = found.manifest.as_component().cloned() else {
        return ResolveComponentResult::Err { error: format!("artifact {hash:?} is not a component") };
    };
    let wasm = match fs::read(&found.path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return ResolveComponentResult::Err { error: format!("reading stored component bytes for {hash:?}: {e}") };
        }
    };
    let config_kind = match config_descriptor(&wasm, export.as_deref()) {
        Ok(config_kind) => config_kind,
        Err(error) => {
            return ResolveComponentResult::Err { error: format!("reading config descriptor for {hash:?}: {error}") };
        }
    };
    ResolveComponentResult::Ok { hash: found.hash, wasm, name: found.name, manifest, export, config_kind }
}

/// Resolve a [`BinarySelector`] against `store` to the stored content
/// bytes the spawn forks (ADR-0115). Resolution order: an exact `query`
/// token wins first (`hash` > `name@version` > `name`); absent a token,
/// the `chassis` / `caps` / `target` attribute query resolves, and with
/// no attribute filters either, `default` = the [`DEFAULT_CHASSIS`]
/// binary. `None` when nothing matched.
pub fn resolve_selector(store: &mut ArtifactStore, selector: &BinarySelector) -> Option<StoredArtifact> {
    if let Some(token) = selector.query.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        // Exact hash wins outright.
        if let Some(found) = store.get(&Selector::Hash(token.to_owned())) {
            return Some(found);
        }
        // `name@version`: the binary's self-reported build id (the
        // manifest `git_sha`) pins a specific entry of a name.
        if let Some((name, version)) = token.split_once('@') {
            let hash = pick_versioned(store, name, version)?;
            return store.get(&Selector::Hash(hash));
        }
        // A bare name points at the latest hash uploaded under it.
        return store.get(&Selector::Name(token.to_owned()));
    }
    // No exact token: an attribute query, else `default` = headless.
    let hash = store.matching_binaries(&attribute_filter(selector)).into_iter().map(|entry| entry.hash).min()?;
    store.get(&Selector::Hash(hash))
}

/// The store filter for a tokenless [`BinarySelector`]: the explicit
/// `chassis` / `caps` / `target` attribute query, or — when none is
/// set — the `default` filter selecting the [`DEFAULT_CHASSIS`]
/// chassis.
fn attribute_filter(selector: &BinarySelector) -> ListEngineBinaries {
    if selector.chassis.is_none() && selector.caps.is_empty() && selector.target.is_none() {
        ListEngineBinaries {
            chassis: Some(DEFAULT_CHASSIS.to_owned()),
            caps: Vec::new(),
            target: None,
            limit: None,
            include_history: false,
        }
    } else {
        ListEngineBinaries {
            chassis: selector.chassis.clone(),
            caps: selector.caps.clone(),
            target: selector.target.clone(),
            limit: None,
            include_history: false,
        }
    }
}

/// The content hash of the entry named `name` whose manifest build id
/// (`git_sha`) is `version` — the `name@version` selector (ADR-0115).
/// `None` when no current entry matches both.
fn pick_versioned(store: &ArtifactStore, name: &str, version: &str) -> Option<String> {
    store
        .matching_binaries(&ListEngineBinaries::default())
        .into_iter()
        .find(|entry| entry.name.as_deref() == Some(name) && entry.manifest.git_sha == version)
        .map(|entry| entry.hash)
}

/// Copy the content bytes at `src` to `dest` and mark `dest`
/// executable (`0o755` on Unix; the `from_mode` precedent in
/// `anthropic/cli.rs`), creating `dest`'s parent dir. The
/// realize-to-exec step for spawn: stored bytes aren't directly
/// fork-exec'able (ADR-0115 §Execution).
pub fn realize_executable(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{bootstrap_ingest, validate_manifest};
    use crate::store::{ArtifactStore, DEFAULT_DISK_BUDGET_BYTES};
    use aether_kinds::BinaryManifest;

    /// A fully conforming manifest: every field the produce side
    /// structurally guarantees is present and non-empty. The baseline the
    /// rejection cases perturb one field at a time.
    fn conforming() -> BinaryManifest {
        BinaryManifest {
            chassis: "headless".to_owned(),
            caps: vec!["aether.rpc.server".to_owned()],
            git_sha: "deadbee".to_owned(),
            profile: "debug".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            env_keys: vec!["AETHER_RPC_PORT".to_owned()],
            argv_flags: vec!["rpc-port".to_owned()],
        }
    }

    fn raw(manifest: &BinaryManifest) -> serde_json::Value {
        serde_json::to_value(manifest).expect("a manifest serializes to a JSON value")
    }

    #[test]
    fn a_conforming_manifest_is_accepted() {
        let manifest = conforming();
        validate_manifest(&manifest, &raw(&manifest)).expect("a fully-shaped manifest conforms");
    }

    /// Tripwire: `build.rs` bakes `"unknown"` (never an empty string) when
    /// a provenance fact is unavailable (built outside a git checkout), so
    /// `"unknown"` is a conforming value — the gate rejects only a truly
    /// empty field. A regression that treated `"unknown"` as missing would
    /// reject every binary built outside a git checkout.
    #[test]
    fn unknown_provenance_is_conforming() {
        let mut manifest = conforming();
        manifest.git_sha = "unknown".to_owned();
        validate_manifest(&manifest, &raw(&manifest)).expect("baked \"unknown\" provenance conforms");
    }

    #[test]
    fn a_manifest_predating_the_config_surface_is_rejected_naming_the_field() {
        // A pre-ADR-0162 manifest: the JSON carries no `env_keys` /
        // `argv_flags`. The typed value defaults them to empty, so the raw
        // JSON is what distinguishes absent (predates the surface) from
        // present-but-empty — the gate must reject the former naming the
        // missing field and the ADR that added it.
        let raw = serde_json::json!({
            "chassis": "headless",
            "caps": ["aether.fs"],
            "git_sha": "deadbee",
            "profile": "debug",
            "target": "x86_64-unknown-linux-gnu"
        });
        let manifest: BinaryManifest =
            serde_json::from_value(raw.clone()).expect("an old-shape manifest parses with defaulted fields");
        let error = validate_manifest(&manifest, &raw).expect_err("a pre-config-surface manifest is rejected");
        assert!(error.contains("env_keys"), "the rejection names the missing field: {error}");
        assert!(error.contains("ADR-0162"), "the rejection names the vintage that must rebuild: {error}");
    }

    #[test]
    fn an_empty_produce_side_guaranteed_field_is_rejected_naming_it() {
        let mut manifest = conforming();
        manifest.caps.clear();
        let error = validate_manifest(&manifest, &raw(&manifest)).expect_err("an empty caps set is rejected");
        assert!(error.contains("caps"), "the rejection names the empty field: {error}");
    }

    #[test]
    fn a_present_but_empty_config_surface_field_is_rejected_naming_it() {
        // Distinct from the absent case: the field *is* in the JSON, just
        // empty. This exercises the non-empty branch (a different message
        // than the predates-the-surface branch), which a real chassis can
        // never produce — the RPC server alone fills each set.
        let mut manifest = conforming();
        manifest.argv_flags.clear();
        let error = validate_manifest(&manifest, &raw(&manifest)).expect_err("an empty argv_flags set is rejected");
        assert!(error.contains("argv_flags"), "the rejection names the empty field: {error}");
    }

    /// `bootstrap_ingest` keeps its log-and-skip contract even now that
    /// ingestion is gated (#3936): a bootstrap entry whose `--describe`
    /// yields a nonconforming manifest is skipped, never a boot failure —
    /// the store stays empty rather than the call panicking. A bad
    /// bootstrap entry must not fail hub boot.
    #[cfg(unix)]
    #[test]
    fn bootstrap_ingest_skips_a_nonconforming_bin_rather_than_failing() {
        use std::collections::HashSet;
        use std::fs::{self, Permissions};
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};
        use std::{env, process};

        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-strict-bootstrap-skip-{}-{nanos}", process::id()));
        fs::create_dir_all(&dir).expect("test setup: temp dir");

        // A stand-in that prints a pre-ADR-0162 manifest on `--describe`
        // (no config surface) — nonconforming, so the upload gate rejects
        // it and bootstrap skips it.
        let stand_in = dir.join("aether-headless");
        fs::write(
            &stand_in,
            "#!/bin/sh\nif [ \"$1\" = \"--describe\" ]; then printf \
                 '{\"chassis\":\"headless\",\"caps\":[\"aether.fs\"],\"git_sha\":\"deadbee\",\
                 \"profile\":\"debug\",\"target\":\"x86_64-unknown-linux-gnu\"}'; fi\n",
        )
        .expect("test setup: write stand-in");
        fs::set_permissions(&stand_in, Permissions::from_mode(0o755)).expect("test setup: chmod");

        let mut store =
            ArtifactStore::open(&dir.join("store"), DEFAULT_DISK_BUDGET_BYTES).expect("test setup: open store");
        bootstrap_ingest(&mut store, &HashSet::from([stand_in.to_string_lossy().into_owned()]));
        assert_eq!(store.entry_count(), 0, "a nonconforming bootstrap bin is skipped, not stored");

        let _ = fs::remove_dir_all(&dir);
    }
}
