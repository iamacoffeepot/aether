//! A module's custom sections, split into a code-shared part and a per-file asset
//! index (ADR-0241 §2, ADR-0250).
//!
//! [`CodeManifest::parse`] runs each code-section reader in [`kind_manifest`]
//! once per code and keeps what it returns, while the per-file side runs the
//! [`asset_manifest`] reader once per file, so no engine reader walks the wasm
//! bytes again after check-in. The section decoders themselves are unchanged:
//! parsing the shared part once per code is the rule, not one wasmparser walk
//! for every section, and not one kind parse per bundle.

use std::collections::HashSet;
use std::sync::Arc;

use aether_actor::NAMESPACE_SEGMENT_MAX_LEN;
use aether_data::canonical::kind_id_from_parts;
use aether_data::{ActorLineageRecord, Blob, CONTENT_ADDRESSED_SECTION, KindDescriptor, KindId};
use aether_kinds::AssetInfo;
use rustc_hash::FxHashMap;

use super::{AssetName, HASH_HEX_BYTES};
use crate::actor::native::BlobCheckIn;
use crate::actor::wasm::asset_manifest;
use crate::actor::wasm::kind_manifest::{self, ActorInputs};

/// The longest namespace a content-addressed module may export: the name it
/// publishes, `<namespace>.<module hash>`, must stay one ADR-0166 segment.
const CONTENT_ADDRESSED_NAMESPACE_MAX_BYTES: usize = NAMESPACE_SEGMENT_MAX_LEN - 1 - HASH_HEX_BYTES;

/// The code-derived part of a module's manifest, shared by every file over one
/// code (ADR-0241 §2): kinds, groups, boot, lineage, namespace, and the
/// content-addressed marker. Read-only: only [`CodeManifest::parse`] builds
/// one, when [`ModuleCache::check_in`](super::ModuleCache::check_in) compiles
/// a new code.
pub(super) struct CodeManifest {
    kinds: Vec<KindDescriptor>,
    kind_ids: HashSet<KindId>,
    actors: Vec<ActorInputs>,
    private_actors: Vec<ActorInputs>,
    boot: Option<String>,
    lineage: Vec<ActorLineageRecord>,
    namespace: Option<String>,
    content_addressed: bool,
}

/// Everything the engine reads from a module's custom sections: the
/// code-shared `CodeManifest` plus the file's own [`AssetIndex`]. Read-only:
/// only [`ModuleCache::check_in`](super::ModuleCache::check_in) builds one,
/// from a shared code part and a per-file asset index.
pub struct ModuleManifest {
    code: Arc<CodeManifest>,
    assets: Arc<AssetIndex>,
}

/// A module's assets, indexed once when the module is published: each asset's
/// own blob, checked in from the module file (ADR-0250 §1).
pub struct AssetIndex {
    /// Section order: what `describe_component` reports.
    catalog: Vec<AssetInfo>,
    /// Section order: each asset's name and own blob.
    sections: Vec<AssetSection>,
    /// An asset's position in both lists, by name.
    by_name: FxHashMap<AssetName, usize>,
}

impl AssetIndex {
    /// Index `records`, each name minted into an [`AssetName`], checking each
    /// record's `offset..offset + len` slice of `file` in through `blobs` as
    /// the asset's own blob. The lists keep the records' order; the map
    /// answers a name in constant time.
    fn new(records: Vec<asset_manifest::AssetRecord>, file: &[u8], blobs: &BlobCheckIn) -> Result<Self, String> {
        let mut sections = Vec::with_capacity(records.len());
        let mut by_name = FxHashMap::default();
        by_name.reserve(records.len());
        for (position, record) in records.iter().enumerate() {
            let name = AssetName::new(&record.info.name)?;
            let end = record.offset.checked_add(record.len).ok_or_else(|| {
                format!("`{}`: the asset's recorded range runs past the module's bytes", record.info.name)
            })?;
            let bytes = file.get(record.offset..end).ok_or_else(|| {
                format!("`{}`: the module's bytes end before the asset's recorded range", record.info.name)
            })?;
            let blob = blobs.check_in(bytes.to_vec().into_boxed_slice());
            by_name.insert(name.clone(), position);
            sections.push(AssetSection { name, blob });
        }
        let catalog = records.into_iter().map(|record| record.info).collect();

        Ok(Self { catalog, sections, by_name })
    }

    /// The asset named `name`: its name and own blob, or `None` when the
    /// module carries none.
    #[must_use]
    pub fn section(&self, name: &str) -> Option<&AssetSection> {
        self.by_name.get(name).map(|&position| &self.sections[position])
    }

    /// Each asset's name and length, in section order.
    #[must_use]
    pub fn catalog(&self) -> &[AssetInfo] {
        &self.catalog
    }
}

/// One asset's own blob, checked in from the module file at publish (ADR-0250
/// §1): the payload the instance's host calls serve.
#[derive(Clone)]
pub struct AssetSection {
    pub name: AssetName,
    pub blob: Blob,
}

impl CodeManifest {
    /// Parse every code-derived section from `code`, with each reader's own
    /// error. Readers run in the order a load reports them: kinds, the
    /// exported and private groups, boot, lineage, and namespace. A
    /// content-addressed module exporting a namespace too long to carry its
    /// hash in one segment is refused, naming it.
    pub(super) fn parse(code: &[u8]) -> Result<Self, String> {
        let kinds = kind_manifest::read_from_bytes(code)?;
        let actors = kind_manifest::read_actor_inputs_from_bytes(code)?;
        let private_actors = kind_manifest::read_private_actor_inputs_from_bytes(code)?;
        let boot = kind_manifest::read_boot_namespace_from_bytes(code)?;
        let lineage = kind_manifest::read_actor_lineage_from_bytes(code)?;
        let namespace = kind_manifest::read_namespace_from_bytes(code)?;

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
            content_addressed: kind_manifest::read_content_addressed_marker(code),
        };

        if manifest.content_addressed {
            let overlong = manifest
                .exported_groups()
                .find(|(namespace, _)| namespace.len() > CONTENT_ADDRESSED_NAMESPACE_MAX_BYTES);

            if let Some((namespace, _)) = overlong {
                return Err(format!(
                    "{CONTENT_ADDRESSED_SECTION}: exported namespace `{namespace}` is {} bytes; a content-addressed \
                     module's namespace is at most {CONTENT_ADDRESSED_NAMESPACE_MAX_BYTES} bytes, so `<namespace>.<module \
                     hash>` fits one {NAMESPACE_SEGMENT_MAX_LEN}-byte segment",
                    namespace.len()
                ));
            }
        }

        Ok(manifest)
    }

    /// Every exported group with its namespace resolved, in declaration
    /// order. A multi-actor module names each group by its boundary record;
    /// the implicit group of a single-actor module takes the module's
    /// namespace, and is skipped when the module declares none. The boot
    /// type's group is exported.
    fn exported_groups(&self) -> impl Iterator<Item = (&str, &ActorInputs)> {
        self.actors.iter().filter_map(|group| {
            group.namespace.as_deref().or(self.namespace.as_deref()).map(|namespace| (namespace, group))
        })
    }

    /// Every private inline child's group with its namespace, in declaration
    /// order. Every private group is led by a boundary record, so one without
    /// a namespace is malformed and skipped.
    fn private_groups(&self) -> impl Iterator<Item = (&str, &ActorInputs)> {
        self.private_actors.iter().filter_map(|group| group.namespace.as_deref().map(|namespace| (namespace, group)))
    }
}

impl ModuleManifest {
    /// Build a manifest from its code-shared part and its file's asset index.
    pub(super) fn from_parts(code: Arc<CodeManifest>, assets: Arc<AssetIndex>) -> Self {
        Self { code, assets }
    }

    /// Index the asset sections of `file`: each asset's catalog entry plus its
    /// own blob checked in from its `offset..offset + len` slice of `file`
    /// through `blobs`. A duplicate section or an empty asset path fails this
    /// file, even when its code is already shared.
    pub(super) fn asset_index(file: &[u8], blobs: &BlobCheckIn) -> Result<Arc<AssetIndex>, String> {
        let records = asset_manifest::read_assets_from_bytes(file)?;
        Ok(Arc::new(AssetIndex::new(records, file, blobs)?))
    }

    /// Every kind the `aether.kinds` section declares, labels merged.
    #[must_use]
    pub fn kinds(&self) -> &[KindDescriptor] {
        &self.code.kinds
    }

    /// The id of every declared kind, derived from its name and schema, so a
    /// reshaped kind reads as a different kind.
    #[must_use]
    pub fn kind_ids(&self) -> &HashSet<KindId> {
        &self.code.kind_ids
    }

    /// Every exported actor type's group, in declaration order; the first is
    /// the entry type.
    #[must_use]
    pub fn actors(&self) -> &[ActorInputs] {
        &self.code.actors
    }

    /// Every private inline child's group (`export!(private = [..])`).
    #[must_use]
    pub fn private_actors(&self) -> &[ActorInputs] {
        &self.code.private_actors
    }

    /// Every exported group with its namespace resolved, in declaration
    /// order. A multi-actor module names each group by its boundary record;
    /// the implicit group of a single-actor module takes [`Self::namespace`],
    /// and is skipped when the module declares none. The boot type's group is
    /// exported.
    pub fn exported_groups(&self) -> impl Iterator<Item = (&str, &ActorInputs)> {
        self.code.exported_groups()
    }

    /// Whether the exported type named `namespace` declares
    /// `#[actor(instanced)]` (ADR-0241 §5), resolved as
    /// [`Self::exported_groups`] resolves names, so a single-actor module's
    /// implicit group answers to the module's namespace. `None` when no
    /// exported group has that name.
    #[must_use]
    pub fn instanced(&self, namespace: &str) -> Option<bool> {
        self.exported_groups().find(|(name, _)| *name == namespace).map(|(_, group)| group.instanced)
    }

    /// Every private inline child's group with its namespace, in declaration
    /// order. Every private group is led by a boundary record, so one without
    /// a namespace is malformed and skipped.
    pub fn private_groups(&self) -> impl Iterator<Item = (&str, &ActorInputs)> {
        self.code.private_groups()
    }

    /// The namespace of the module's boot type (ADR-0147), if it declares one.
    #[must_use]
    pub fn boot(&self) -> Option<&str> {
        self.code.boot.as_deref()
    }

    /// The placement records of the module's actor types.
    #[must_use]
    pub fn lineage(&self) -> &[ActorLineageRecord] {
        &self.code.lineage
    }

    /// The module's `aether.namespace` section: a single-actor module's
    /// namespace.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.code.namespace.as_deref()
    }

    /// Whether the module carries the ADR-0241 §3 content-addressed marker, so
    /// each namespace it exports publishes qualified by its hash
    /// ([`Module::published_groups`](super::Module::published_groups)).
    #[must_use]
    pub fn content_addressed(&self) -> bool {
        self.code.content_addressed
    }

    /// Each asset's name and length, in section order: the catalog
    /// `describe_component` reports.
    #[must_use]
    pub fn asset_catalog(&self) -> &[AssetInfo] {
        self.assets.catalog()
    }

    /// The module's asset index, held for as long as the module lives.
    #[must_use]
    pub fn assets(&self) -> &Arc<AssetIndex> {
        &self.assets
    }
}
