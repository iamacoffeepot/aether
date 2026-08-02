//! The composition-derived chassis config aggregate (ADR-0156 §4): the union
//! of every composed member's declaration, and the discovery surfaces that
//! read it — the known-keys sweep, the argv-flag inventory, the
//! `--print-config` dump, and the per-member provenance rollup.

use std::collections::BTreeSet;

use confique::meta::Meta;

use super::dump::dump_config;
use super::known_keys::{KnobRecord, KnownKeys, known_keys};
use super::member::ConfigMemberRecord;
use super::sources::{ConfigProvenance, ConfigSources};

/// The composition-derived chassis config aggregate (ADR-0156 §4): the
/// union of every composed cap's [`ConfigMember`](crate::ConfigMember) declaration, the driver's,
/// and the chassis-declared non-cap members (workers / ring capacities /
/// scheduler tuning / teardown budget), assembled by
/// [`Builder::config_manifest`](crate::chassis::builder::Builder::config_manifest).
/// The known-keys sweep and `--print-config` dump read this walk instead of a
/// hand-maintained registry, so a chassis knows exactly the knobs it composes
/// — headless stops "knowing" the window/audio knobs it never wires, desktop
/// stops "knowing" the headless tick knob.
#[derive(Clone, Debug, Default)]
pub struct ConfigManifest {
    members: Vec<ConfigMemberRecord>,
}

impl ConfigManifest {
    /// Assemble a manifest from a member list (the builder's accumulator
    /// plus the driver's members).
    #[must_use]
    pub fn from_members(members: Vec<ConfigMemberRecord>) -> Self {
        Self { members }
    }

    /// The declared members, in composition order.
    #[must_use]
    pub fn members(&self) -> &[ConfigMemberRecord] {
        &self.members
    }

    /// Every member's confique `Meta`, for the [`known_keys`] / [`dump_config`]
    /// walks.
    #[must_use]
    pub fn metas(&self) -> Vec<&'static Meta> {
        self.members.iter().map(|record| record.meta).collect()
    }

    /// The file-section vocabulary the composed members declare.
    pub fn sections(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.members.iter().map(|record| record.section)
    }

    /// Assemble the [`KnownKeys`] set for the unknown-`AETHER_*` sweep from
    /// this walk plus the residual hand-registered knobs (the chassis-direct
    /// records the aggregate doesn't yet own — the RPC port, frame size,
    /// runtime log/panic knobs; ADR-0156 §6 folds those in on later slices).
    #[must_use]
    pub fn known_keys(&self, records: &[KnobRecord]) -> KnownKeys {
        known_keys(&self.metas(), records)
    }

    /// The composition-derived argv overlay surface (ADR-0162): every composed
    /// member's derive-emitted long flags, deduped and sorted. The
    /// [`BinaryManifest`](aether_kinds::BinaryManifest)'s `argv_flags` reports
    /// this — the machine-channel flags a spawn-side validator checks an
    /// injected `--flag` against — read from the same composition a real boot
    /// runs, never a parallel hand list. Residual hand-registered knobs
    /// ([`KnobRecord`]) carry env keys but no derive-emitted flag, so they
    /// contribute nothing here (they fold into [`known_keys`](Self::known_keys)
    /// alone).
    #[must_use]
    pub fn argv_flags(&self) -> Vec<&'static str> {
        let mut flags: BTreeSet<&'static str> = BTreeSet::new();
        for member in &self.members {
            flags.extend(member.cli_flags.iter().copied());
        }
        flags.into_iter().collect()
    }

    /// Render the `--print-config` discovery dump from this walk plus the
    /// residual hand-registered knobs.
    #[must_use]
    pub fn dump(&self, records: &[KnobRecord]) -> String {
        dump_config(&self.metas(), records)
    }

    /// ADR-0156 §5: the resolved-provenance rollup — for every declared member,
    /// the highest-precedence source layer present in `sources` (which
    /// programmatic / argv / file / env / default supplied its value). Sibling
    /// to [`dump`](Self::dump)'s per-key value table: `dump` renders each
    /// knob's live value, this attributes each member to its winning layer.
    ///
    /// Reads the same stack the builder resolves cap configs against, so the
    /// rollup reflects exactly what a boot would resolve. The member's section
    /// pairs with the provenance so callers can render a `section = layer`
    /// line.
    #[must_use]
    pub fn provenance(&self, sources: &ConfigSources) -> Vec<(&'static str, ConfigProvenance)> {
        self.members
            .iter()
            .map(|record| (record.section, sources.provenance(record.type_id, record.section, record.meta)))
            .collect()
    }
}
