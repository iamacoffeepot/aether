//! Artifact vocabulary — kind tags, manifest types, selectors, and the
//! wasm component manifest reader (ADR-0116).

use std::path::PathBuf;

use aether_kinds::{
    BinaryManifest, ComponentActor, ComponentManifest, ListComponentBinaries, ListEngineBinaries,
};
use serde::{Deserialize, Serialize};

/// The type tag on a stored artifact (ADR-0115 / ADR-0116). The store is
/// artifact-generic: an entry's `kind` selects which [`StoredManifest`]
/// variant describes it, so binaries and component wasm share one store
/// (#1955 can add more — asset bundles — without reshaping the entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// pub(crate) is its true minimal reach (re-exported / used across the crate's modules); redundant_pub_crate sees only the private-module ancestor.
#[allow(clippy::redundant_pub_crate)]
pub(crate) enum ArtifactKind {
    /// A chassis substrate binary, described by a [`BinaryManifest`].
    Binary,
    /// A wasm component, described by a [`ComponentManifest`] (ADR-0116).
    Component,
}

/// The type-tagged manifest a stored artifact carries (ADR-0115 /
/// ADR-0116). The store sidecars one of these next to each entry's bytes;
/// the variant matches the entry's [`ArtifactKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// pub(crate) is its true minimal reach (re-exported / used across the crate's modules); redundant_pub_crate sees only the private-module ancestor.
#[allow(clippy::redundant_pub_crate)]
pub(crate) enum StoredManifest {
    Binary(BinaryManifest),
    Component(ComponentManifest),
}

impl StoredManifest {
    /// The [`BinaryManifest`] when this artifact is a binary, else `None`.
    #[must_use]
    pub(crate) fn as_binary(&self) -> Option<&BinaryManifest> {
        match self {
            Self::Binary(m) => Some(m),
            Self::Component(_) => None,
        }
    }

    /// The [`ComponentManifest`] when this artifact is a component, else
    /// `None`.
    #[must_use]
    pub(crate) fn as_component(&self) -> Option<&ComponentManifest> {
        match self {
            Self::Component(m) => Some(m),
            Self::Binary(_) => None,
        }
    }
}

/// How a caller addresses a stored artifact in [`super::ArtifactStore::get`] —
/// by its content hash or by a human-readable name. The seam #1954's
/// spawn cutover consumes to resolve a registry reference to bytes.
#[derive(Debug, Clone)]
// pub(crate) is its true minimal reach (re-exported / used across the crate's modules); redundant_pub_crate sees only the private-module ancestor.
#[allow(clippy::redundant_pub_crate)]
pub(crate) enum Selector {
    /// The sha256 hex content address.
    Hash(String),
    /// A name an upload pointed at a hash.
    Name(String),
}

/// One resolved artifact returned by [`super::ArtifactStore::get`]: its content
/// hash, the on-disk path of its raw bytes (the fork target for #1954, the
/// resolve-and-forward byte source for #1956), the type tag, the
/// type-tagged manifest, and the name pointing at it (if any).
#[derive(Debug, Clone)]
// pub(crate) is its true minimal reach (re-exported / used across the crate's modules); redundant_pub_crate sees only the private-module ancestor.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct StoredArtifact {
    pub hash: String,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub kind: ArtifactKind,
    pub manifest: StoredManifest,
    pub name: Option<String>,
}

/// Whether a binary manifest passes a [`ListEngineBinaries`] filter.
pub(super) fn matches_binary_filter(
    manifest: &BinaryManifest,
    filter: &ListEngineBinaries,
) -> bool {
    if let Some(chassis) = &filter.chassis
        && &manifest.chassis != chassis
    {
        return false;
    }
    if let Some(target) = &filter.target
        && &manifest.target != target
    {
        return false;
    }
    filter.caps.iter().all(|c| manifest.caps.contains(c))
}

/// Whether a component manifest passes a [`ListComponentBinaries`] filter
/// (ADR-0116): the optional `namespace` must be one of the exported actor
/// namespaces, and the optional `handled_kind` must be in the manifest's
/// handled-kind union. Each absent field is "no constraint".
pub(super) fn matches_component_filter(
    manifest: &ComponentManifest,
    filter: &ListComponentBinaries,
) -> bool {
    if let Some(namespace) = &filter.namespace
        && !manifest.namespaces.iter().any(|n| n == namespace)
    {
        return false;
    }
    if let Some(handled_kind) = &filter.handled_kind
        && !manifest.handled_kinds.contains(handled_kind)
    {
        return false;
    }
    true
}

/// Read a component's [`ComponentManifest`] straight from its wasm bytes
/// (ADR-0116, issue 1956) — no execution step. Reuses the substrate's
/// `wasmparser`-based section readers (the same ones the substrate uses at
/// load): `read_actor_inputs_from_bytes` for the exported actor groups and
/// their handled kind ids + `#[fallback]` presence (`aether.kinds.inputs`,
/// ADR-0033 / ADR-0096), `read_namespace_from_bytes` for a single-actor
/// module's `aether.namespace`, and `read_producers_from_bytes` for build
/// provenance. The hub indexes a component by what it self-reports.
///
/// Each [`ComponentActor`]'s `namespace` is the group's `Addressable::NAMESPACE`
/// from its `ActorBoundary` record; a single-actor module's implicit group
/// (`namespace: None`) takes the module's `aether.namespace` section value.
/// The top-level `handled_kinds` / `fallback` are the union across every
/// exported actor.
///
/// # Errors
///
/// Returns the section reader's error string when the wasm can't be parsed
/// (a malformed `aether.kinds.inputs` section). A component with no inputs
/// section yields an empty manifest with whatever namespace / provenance is
/// present.
// pub(crate) is its true minimal reach (re-exported / used across the crate's modules); redundant_pub_crate sees only the private-module ancestor.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn component_manifest(wasm: &[u8]) -> Result<ComponentManifest, String> {
    use aether_substrate::actor::wasm::kind_manifest;

    let groups = kind_manifest::read_actor_inputs_from_bytes(wasm)?;
    let module_namespace = kind_manifest::read_namespace_from_bytes(wasm)?;
    let provenance = kind_manifest::read_producers_from_bytes(wasm);

    let mut actors: Vec<ComponentActor> = Vec::with_capacity(groups.len());
    for group in groups {
        // A boundary-named group carries its own `Addressable::NAMESPACE`; a
        // single-actor module's implicit group (`None`) resolves its name
        // from the `aether.namespace` section, falling back to empty.
        let namespace = group
            .namespace
            .or_else(|| module_namespace.clone())
            .unwrap_or_default();
        let handled_kinds: Vec<aether_data::KindId> =
            group.capabilities.handlers.iter().map(|h| h.id).collect();
        actors.push(ComponentActor {
            namespace,
            handled_kinds,
            fallback: group.capabilities.fallback.is_some(),
        });
    }

    let namespaces: Vec<String> = actors.iter().map(|a| a.namespace.clone()).collect();
    // The handled-kind union across every exported actor, deduped (the
    // selector axis "a component that handles K"). A `Vec` is right here —
    // a component handles a handful of kinds.
    let mut handled_kinds: Vec<aether_data::KindId> = Vec::new();
    for actor in &actors {
        for id in &actor.handled_kinds {
            if !handled_kinds.contains(id) {
                handled_kinds.push(*id);
            }
        }
    }
    let fallback = actors.iter().any(|a| a.fallback);

    Ok(ComponentManifest {
        namespaces,
        actors,
        handled_kinds,
        fallback,
        provenance,
    })
}
