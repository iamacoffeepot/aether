//! Artifact vocabulary — kind tags, manifest types, selectors, and the
//! wasm component manifest reader (ADR-0116).

use std::path::PathBuf;

use aether_data::{canonical::kind_id_from_parts, wire};
use aether_kinds::{
    BinaryManifest, ComponentActor, ComponentManifest, KindDescriptorWire, ListComponentBinaries, ListEngineBinaries,
};
use serde::{Deserialize, Serialize};

/// The type tag on a stored artifact (ADR-0115 / ADR-0116). The store is
/// artifact-generic: an entry's `kind` selects which [`StoredManifest`]
/// variant describes it, so binaries and component wasm share one store
/// (#1955 can add more — asset bundles — without reshaping the entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    /// A chassis substrate binary, described by a [`BinaryManifest`].
    Binary,
    /// A wasm component, described by a [`ComponentManifest`] (ADR-0116).
    Component,
}

/// The type-tagged manifest a stored artifact carries (ADR-0115 /
/// ADR-0116). The store sidecars one of these next to each entry's bytes;
/// the variant matches the entry's [`ArtifactKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredManifest {
    Binary(BinaryManifest),
    Component(ComponentManifest),
}

impl StoredManifest {
    /// The [`BinaryManifest`] when this artifact is a binary, else `None`.
    #[must_use]
    pub fn as_binary(&self) -> Option<&BinaryManifest> {
        match self {
            Self::Binary(m) => Some(m),
            Self::Component(_) => None,
        }
    }

    /// The [`ComponentManifest`] when this artifact is a component, else
    /// `None`.
    #[must_use]
    pub fn as_component(&self) -> Option<&ComponentManifest> {
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
pub enum Selector {
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
pub struct StoredArtifact {
    pub hash: String,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub kind: ArtifactKind,
    pub manifest: StoredManifest,
    pub name: Option<String>,
}

/// Whether a binary manifest passes a [`ListEngineBinaries`] filter.
pub fn matches_binary_filter(manifest: &BinaryManifest, filter: &ListEngineBinaries) -> bool {
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
pub fn matches_component_filter(manifest: &ComponentManifest, filter: &ListComponentBinaries) -> bool {
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
pub fn component_manifest(wasm: &[u8]) -> Result<ComponentManifest, String> {
    use aether_substrate::actor::wasm::kind_manifest;

    let groups = kind_manifest::read_actor_inputs_from_bytes(wasm)?;
    let module_namespace = kind_manifest::read_namespace_from_bytes(wasm)?;
    let provenance = kind_manifest::read_producers_from_bytes(wasm);
    // ADR-0138: a defaultless multi-actor module carries the no-default
    // marker and omits `aether.namespace`, so it has no bare-load default.
    // Otherwise the default is the module's `aether.namespace` value (the
    // single-actor namespace or the `export!(default = …)` opt-in).
    let default_entry = if kind_manifest::read_no_default_marker(wasm) {
        None
    } else {
        module_namespace.clone()
    };

    let mut actors: Vec<ComponentActor> = Vec::with_capacity(groups.len());
    for group in groups {
        // A boundary-named group carries its own `Addressable::NAMESPACE`; a
        // single-actor module's implicit group (`None`) resolves its name
        // from the `aether.namespace` section, falling back to empty.
        let namespace = group.namespace.or_else(|| module_namespace.clone()).unwrap_or_default();
        let handled_kinds: Vec<aether_data::KindId> = group.capabilities.handlers.iter().map(|h| h.id).collect();
        actors.push(ComponentActor { namespace, handled_kinds, fallback: group.capabilities.fallback.is_some() });
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

    Ok(ComponentManifest { namespaces, actors, handled_kinds, fallback, provenance, default_entry })
}

/// Read the component's typed init-config descriptor from its wasm bytes.
/// The persisted [`ComponentManifest`] intentionally stores only ids/names;
/// resolve computes this full descriptor from the same wasm bytes the store
/// already holds so aether-mcp can JSON-encode config before load/boot.
pub fn config_descriptor(wasm: &[u8], export: Option<&str>) -> Result<Option<KindDescriptorWire>, String> {
    use aether_substrate::actor::wasm::kind_manifest;

    let capabilities = match export {
        Some(export) => kind_manifest::read_actor_inputs_from_bytes(wasm)?
            .into_iter()
            .find(|actor| actor.namespace.as_deref() == Some(export))
            .map(|actor| actor.capabilities)
            .ok_or_else(|| format!("export {export:?} is not declared in the component"))?,
        None => kind_manifest::read_inputs_from_bytes(wasm)?,
    };
    let Some(config) = capabilities.config else {
        return Ok(None);
    };

    let descriptors = kind_manifest::read_from_bytes(wasm)?;
    let Some(descriptor) = descriptors
        .into_iter()
        .find(|descriptor| kind_id_from_parts(&descriptor.name, &descriptor.schema) == config.id.0)
    else {
        return Err(format!(
            "component declares config kind {} ({}) but its schema descriptor is absent",
            config.name, config.id
        ));
    };

    let schema_wire =
        wire::to_vec(&descriptor.schema).map_err(|e| format!("encoding config schema for {}: {e}", descriptor.name))?;
    Ok(Some(KindDescriptorWire { id: config.id, name: descriptor.name, schema_wire }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a minimal wasm carrying one custom section (the section
    /// reader only needs the sections; no functions), mirroring the
    /// substrate's own `kind_manifest` section-reader test fixtures.
    fn wasm_with_section(section_name: &str, section: &[u8]) -> Vec<u8> {
        use core::fmt::Write as _;
        let mut escaped = String::with_capacity(section.len() * 3);
        for b in section {
            write!(&mut escaped, "\\{b:02x}").expect("write to String");
        }
        let wat = format!(r#"(module (@custom "{section_name}" "{escaped}") (func (export "noop")))"#);
        wat::parse_str(wat).expect("valid wat")
    }

    #[test]
    fn default_entry_tracks_the_no_default_marker() {
        // ADR-0138: a single-actor / opted-in module carries its
        // `aether.namespace` and no marker, so its bare-load default is that
        // namespace. A defaultless multi-actor module carries the
        // `aether.no_default` marker (and omits `aether.namespace`), so it
        // has no bare-load default.
        let with_default = wasm_with_section("aether.namespace", b"aether.probe");
        assert_eq!(
            component_manifest(&with_default).expect("manifest reads").default_entry.as_deref(),
            Some("aether.probe"),
        );

        let defaultless = wasm_with_section("aether.no_default", &[1u8]);
        assert_eq!(component_manifest(&defaultless).expect("manifest reads").default_entry, None,);
    }
}
