//! Binary / component artifact resolution + ingestion for the engines
//! cap (ADR-0115 / ADR-0116). The content-addressed store seams the
//! handlers delegate to: ingest an uploaded binary / component, resolve
//! a [`BinarySelector`] / `ComponentSelector` to stored bytes, and
//! realize stored bytes to an executable temp file for fork+exec.
//! Native-only (forks `--describe`, reads / copies files).

use crate::store::{
    ArtifactKind, ArtifactStore, Selector, StoredArtifact, StoredManifest, component_manifest,
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
/// its chassis kind, linked caps, and build provenance — without the
/// hub linking the chassis crate. `stdin` is nulled so a describe can't
/// block on input.
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
    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parsing {binary_path:?} --describe manifest JSON: {e}"))
}

/// Ingest the binary at `path` into `store` content-addressed,
/// capturing its manifest via a one-time `<path> --describe` fork
/// (ADR-0115, issue 1953). Shared by the `on_upload_binary` handler and
/// the [`bootstrap_ingest`] boot path. Returns the stored content hash,
/// or a human-readable error for an unreadable path or a `--describe`
/// that failed / yielded no parseable manifest. Idempotent — identical
/// bytes dedup to the same hash.
pub(super) fn ingest_binary(
    store: &mut ArtifactStore,
    path: &str,
    name: Option<String>,
) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("reading binary path {path:?}: {e}"))?;
    let manifest = describe_binary(path)?;
    Ok(store.upload(
        &bytes,
        ArtifactKind::Binary,
        StoredManifest::Binary(manifest),
        name,
    ))
}

/// Bootstrap-ingest each chassis bin in `paths` into `store`, naming
/// each by its file stem so a `default` / `name` selector resolves in a
/// fresh or `restart-hub`'d hub (ADR-0115, issue 1954). The list rides
/// `EngineConfig`'s `binary_bootstrap` field (its `AETHER_BINARY_BOOTSTRAP`
/// env layer, ADR-0090). A path that can't be read or `--describe`d is
/// logged and skipped — a bad bootstrap entry must not fail hub boot.
/// Idempotent via content dedup.
pub(super) fn bootstrap_ingest(store: &mut ArtifactStore, paths: &HashSet<String>) {
    for path_str in paths {
        let name = Path::new(path_str)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned);
        match ingest_binary(store, path_str, name) {
            Ok(hash) => tracing::info!(
                target: "aether_substrate::engine_server",
                path = path_str.as_str(),
                hash = %hash,
                "binary bootstrap: ingested a chassis bin",
            ),
            Err(error) => tracing::warn!(
                target: "aether_substrate::engine_server",
                path = path_str.as_str(),
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
pub(super) fn ingest_component(
    store: &mut ArtifactStore,
    path: &str,
    name: Option<String>,
) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("reading component path {path:?}: {e}"))?;
    let manifest = component_manifest(&bytes)
        .map_err(|e| format!("reading component manifest from {path:?}: {e}"))?;
    Ok(store.upload(
        &bytes,
        ArtifactKind::Component,
        StoredManifest::Component(manifest),
        name,
    ))
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
pub(super) fn resolve_component(
    store: &mut ArtifactStore,
    selector: &ComponentSelector,
) -> ResolveComponentResult {
    // An exact token, with the `@actor` half (if any) split off as the
    // export selector forwarded to the substrate.
    if let Some(token) = selector
        .query
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return resolve_component_token(store, token);
    }
    // No exact token: a namespace / handled-kind attribute query. A
    // match-more-than-one is a clean ambiguity error.
    let mut matches = store.list_components(&ListComponentBinaries {
        namespace: selector.namespace.clone(),
        handled_kind: selector.handled_kind,
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
        return ResolveComponentResult::Err {
            error: format!("no stored component matches the selector {token:?}"),
        };
    }
    // A bare token: an exact hash wins, else a name (latest).
    if store.contains(token) {
        return stored_component_reply(store, token, None);
    }
    if let Some(found) = store.get(&Selector::Name(token.to_owned())) {
        return stored_component_reply(store, &found.hash, None);
    }
    ResolveComponentResult::Err {
        error: format!("no stored component matches the selector {token:?}"),
    }
}

/// Read the stored component `hash`'s wasm bytes + manifest off disk and
/// build a `ResolveComponentResult::Ok` (ADR-0116). `export` threads a
/// `module@actor` selector's actor half through to the forwarded
/// `LoadComponent.export`. An entry that isn't a component (a binary
/// hash) or whose bytes can't be read is a clean `Err`.
fn stored_component_reply(
    store: &mut ArtifactStore,
    hash: &str,
    export: Option<String>,
) -> ResolveComponentResult {
    let Some(found) = store.get(&Selector::Hash(hash.to_owned())) else {
        return ResolveComponentResult::Err {
            error: format!("no stored artifact has hash {hash:?}"),
        };
    };
    let Some(manifest) = found.manifest.as_component().cloned() else {
        return ResolveComponentResult::Err {
            error: format!("artifact {hash:?} is not a component"),
        };
    };
    let wasm = match fs::read(&found.path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return ResolveComponentResult::Err {
                error: format!("reading stored component bytes for {hash:?}: {e}"),
            };
        }
    };
    ResolveComponentResult::Ok {
        hash: found.hash,
        wasm,
        name: found.name,
        manifest,
        export,
    }
}

/// Resolve a [`BinarySelector`] against `store` to the stored content
/// bytes the spawn forks (ADR-0115). Resolution order: an exact `query`
/// token wins first (`hash` > `name@version` > `name`); absent a token,
/// the `chassis` / `caps` / `target` attribute query resolves, and with
/// no attribute filters either, `default` = the [`DEFAULT_CHASSIS`]
/// binary. `None` when nothing matched.
pub(super) fn resolve_selector(
    store: &mut ArtifactStore,
    selector: &BinarySelector,
) -> Option<StoredArtifact> {
    if let Some(token) = selector
        .query
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
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
    let hash = store
        .list_binaries(&attribute_filter(selector))
        .into_iter()
        .map(|entry| entry.hash)
        .min()?;
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
        }
    } else {
        ListEngineBinaries {
            chassis: selector.chassis.clone(),
            caps: selector.caps.clone(),
            target: selector.target.clone(),
        }
    }
}

/// The content hash of the entry named `name` whose manifest build id
/// (`git_sha`) is `version` — the `name@version` selector (ADR-0115).
/// `None` when no current entry matches both.
fn pick_versioned(store: &ArtifactStore, name: &str, version: &str) -> Option<String> {
    store
        .list_binaries(&ListEngineBinaries::default())
        .into_iter()
        .find(|entry| entry.name.as_deref() == Some(name) && entry.manifest.git_sha == version)
        .map(|entry| entry.hash)
}

/// Copy the content bytes at `src` to `dest` and mark `dest`
/// executable (`0o755` on Unix; the `from_mode` precedent in
/// `anthropic/cli.rs`), creating `dest`'s parent dir. The
/// realize-to-exec step for spawn: stored bytes aren't directly
/// fork-exec'able (ADR-0115 §Execution).
pub(super) fn realize_executable(src: &Path, dest: &Path) -> io::Result<()> {
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
