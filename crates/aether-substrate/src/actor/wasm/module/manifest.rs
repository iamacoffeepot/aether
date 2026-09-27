//! A module's custom sections, parsed once per content hash (ADR-0241 §2).
//!
//! [`ModuleManifest::parse`] runs each existing section reader in
//! [`kind_manifest`] and [`asset_manifest`] once and keeps what they return,
//! so no engine reader walks the wasm bytes again after check-in. The section
//! decoders themselves are unchanged: parsing once per hash is the rule, not
//! one wasmparser walk for every section.

use std::collections::HashSet;
use std::ops::Range;

use aether_data::canonical::kind_id_from_parts;
use aether_data::{ActorLineageRecord, KindDescriptor, KindId};
use aether_kinds::AssetInfo;

use super::AssetName;
use crate::actor::wasm::asset_manifest;
use crate::actor::wasm::kind_manifest::{self, ActorInputs};

/// Everything the engine reads from a module's custom sections. Read-only:
/// only [`ModuleManifest::parse`] builds one, when a module is checked in.
pub struct ModuleManifest {
    kinds: Vec<KindDescriptor>,
    kind_ids: HashSet<KindId>,
    actors: Vec<ActorInputs>,
    private_actors: Vec<ActorInputs>,
    boot: Option<String>,
    lineage: Vec<ActorLineageRecord>,
    namespace: Option<String>,
    no_default: bool,
    asset_catalog: Vec<AssetInfo>,
}

/// Where one asset's payload sits in the wasm bytes, for the cache to check
/// it in as its own blob.
pub(super) struct AssetSection {
    pub(super) name: AssetName,
    pub(super) range: Range<usize>,
}

impl ModuleManifest {
    /// Parse every section the engine reads from `wasm`, with each reader's
    /// own error. Readers run in the order a load reports them: kinds, the
    /// exported and private groups, boot, lineage, namespace, then assets.
    /// The asset sections come back beside the manifest, as the byte ranges
    /// the cache checks in.
    pub(super) fn parse(wasm: &[u8]) -> Result<(Self, Vec<AssetSection>), String> {
        let kinds = kind_manifest::read_from_bytes(wasm)?;
        let actors = kind_manifest::read_actor_inputs_from_bytes(wasm)?;
        let private_actors = kind_manifest::read_private_actor_inputs_from_bytes(wasm)?;
        let boot = kind_manifest::read_boot_namespace_from_bytes(wasm)?;
        let lineage = kind_manifest::read_actor_lineage_from_bytes(wasm)?;
        let namespace = kind_manifest::read_namespace_from_bytes(wasm)?;
        let records = asset_manifest::read_assets_from_bytes(wasm)?;

        let sections = records
            .iter()
            .map(|record| {
                Ok(AssetSection {
                    name: AssetName::new(&record.info.name)?,
                    range: record.offset..record.offset + record.len,
                })
            })
            .collect::<Result<_, String>>()?;
        let kind_ids =
            kinds.iter().map(|descriptor| KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema))).collect();

        let manifest = Self {
            kinds,
            kind_ids,
            actors,
            private_actors,
            boot,
            lineage,
            namespace,
            no_default: kind_manifest::read_no_default_marker(wasm),
            asset_catalog: records.into_iter().map(|record| record.info).collect(),
        };
        Ok((manifest, sections))
    }

    /// Every kind the `aether.kinds` section declares, labels merged.
    #[must_use]
    pub fn kinds(&self) -> &[KindDescriptor] {
        &self.kinds
    }

    /// The id of every declared kind, derived from its name and schema, so a
    /// reshaped kind reads as a different kind.
    #[must_use]
    pub fn kind_ids(&self) -> &HashSet<KindId> {
        &self.kind_ids
    }

    /// Every exported actor type's group, in declaration order; the first is
    /// the entry type.
    #[must_use]
    pub fn actors(&self) -> &[ActorInputs] {
        &self.actors
    }

    /// Every private inline child's group (`export!(private = [..])`).
    #[must_use]
    pub fn private_actors(&self) -> &[ActorInputs] {
        &self.private_actors
    }

    /// The namespace of the module's boot type (ADR-0147), if it declares one.
    #[must_use]
    pub fn boot(&self) -> Option<&str> {
        self.boot.as_deref()
    }

    /// The placement records of the module's actor types.
    #[must_use]
    pub fn lineage(&self) -> &[ActorLineageRecord] {
        &self.lineage
    }

    /// The module's `aether.namespace` section: a single-actor module's
    /// namespace, or a multi-actor module's default.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Whether the module carries the ADR-0138 no-default marker.
    #[must_use]
    pub fn no_default(&self) -> bool {
        self.no_default
    }

    /// Each asset's name, length and sha256, in section order: the catalog
    /// `describe_component` reports.
    #[must_use]
    pub fn asset_catalog(&self) -> &[AssetInfo] {
        &self.asset_catalog
    }
}
