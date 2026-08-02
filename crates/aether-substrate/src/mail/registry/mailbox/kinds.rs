//! The kind table: what a `KindId` is registered as, and the name /
//! descriptor reads every wire and render site resolves through.

use std::collections::HashMap;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use aether_data::KindDescriptor;

use crate::mail::KindId;
use crate::mail::registry::authority::BootAuthority;
use crate::mail::registry::effect::{RegistryApplied, RegistryEffect, RegistryEffectError, bytes_kind};
use crate::mail::registry::errors::KindConflict;

use super::Registry;

/// One kind's bookkeeping, keyed in the registry on the hashed id.
#[derive(Clone)]
pub(super) struct KindSlot {
    pub(super) name: Arc<str>,
    pub(super) descriptor: KindDescriptor,
}

#[derive(Clone, Default)]
pub(super) struct KindTable {
    pub(super) kinds: FxHashMap<KindId, KindSlot>,
    pub(super) name_index: HashMap<String, KindId>,
}

impl Registry {
    /// Register a mail kind by name, defaulting the schema to `Bytes`
    /// (raw byte payload, no agent-encodable structure). The id is
    /// derived from `(name, SchemaType::Bytes)` — so the name-only path
    /// only collides with a `register_kind_with_descriptor` call that
    /// also uses the `Bytes` schema. Mostly a convenience for tests and
    /// substrate-internal registrations that don't need the hub to
    /// encode params; production init should prefer
    /// `register_kind_with_descriptor` so the descriptor stored here
    /// matches the type definition and the derived id agrees with
    /// `<K as Kind>::ID` on the guest side.
    ///
    /// # Panics
    /// Panics if the inner routing lock is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard. The internal `expect("Bytes default cannot produce a
    /// conflict")` is unreachable by construction.
    ///
    /// Direct write path — takes a [`BootAuthority`] like its descriptor
    /// sibling (iamacoffeepot/aether#4161); `load_component` stages a
    /// `RegistryBatch::register_kinds` through the ADR-0165 owner instead.
    pub fn register_kind(&self, authority: &BootAuthority, name: impl Into<String>) -> KindId {
        let descriptor = bytes_kind(name.into());
        // A fresh `Bytes` descriptor can only conflict with a prior
        // `Bytes` registration under the same name — in which case the
        // schemas match and the call is idempotent. Not reachable.
        self.register_kind_internal(authority, descriptor, /*reject_conflict=*/ false)
            .expect("Bytes default cannot produce a conflict")
    }

    /// Register a mail kind along with the descriptor the hub will
    /// use to encode agent-supplied params (ADR-0007). Per ADR-0030
    /// Phase 2:
    ///
    /// - Fresh `(name, schema)` hash → insert, return the id.
    /// - Existing id with identical descriptor → return the id
    ///   (idempotent — same kind registered twice, e.g. boot + load).
    /// - Existing id with a different descriptor → `KindConflict`. At
    ///   64-bit hash width this is only reachable via a genuine hash
    ///   collision between two distinct kinds; loud failure rather
    ///   than silent data corruption.
    ///
    /// Used by substrate boot (`descriptors::all()`). Direct write path —
    /// takes a [`BootAuthority`] so only boot can name it
    /// (iamacoffeepot/aether#4156); `load_component` stages a
    /// `RegistryBatch::register_kinds` through the ADR-0165 owner instead.
    ///
    /// # Panics
    /// Panics if the inner routing lock is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn register_kind_with_descriptor(
        &self,
        authority: &BootAuthority,
        descriptor: KindDescriptor,
    ) -> Result<KindId, KindConflict> {
        self.register_kind_internal(authority, descriptor, /*reject_conflict=*/ true)
    }

    fn register_kind_internal(
        &self,
        authority: &BootAuthority,
        descriptor: KindDescriptor,
        reject_conflict: bool,
    ) -> Result<KindId, KindConflict> {
        match self.apply_one(authority, RegistryEffect::RegisterKind { descriptor, reject_conflict }) {
            Ok(RegistryApplied::Kind(id)) => Ok(id),
            Err(RegistryEffectError::Kind(error)) => Err(error),
            Ok(_) | Err(_) => unreachable!("register-kind returns a kind id or kind conflict"),
        }
    }

    /// Look up a kind's id by its canonical name. Under hashed ids the
    /// id is a function of `(name, schema)` — so this only finds a
    /// match if `register_kind_with_descriptor` was called with the
    /// exact descriptor the caller is thinking of. Primarily used by
    /// the hub-inbound dispatch path, which needs to convert an
    /// incoming `kind_name` back to the registered id.
    pub fn kind_id(&self, name: &str) -> Option<KindId> {
        self.kinds.load().table().name_index.get(name).copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// isn't registered. Used by the dispatch path to hand mailbox
    /// closure handlers a kind name without them keeping their own
    /// map.
    pub fn kind_name(&self, kind: KindId) -> Option<String> {
        self.kind_name_shared(kind).map(|name| name.to_string())
    }

    /// Crate-private shared projection for dispatch paths that must not
    /// reallocate the immutable registered kind name.
    pub(crate) fn kind_name_shared(&self, kind: KindId) -> Option<Arc<str>> {
        self.kinds.load().table().kinds.get(&kind).map(|slot| Arc::clone(&slot.name))
    }

    /// A human-readable label for a kind, for diagnostics.
    ///
    /// The registered name when there is one, else the tagged id — so a render
    /// site always has something to print and never has to decide what to do
    /// about an unregistered kind. This is the call the dispatch path used to
    /// pre-empt by carrying an `Arc<str>` through every mail
    /// (iamacoffeepot/aether#4278); made here, at the moment something is
    /// actually being written, it costs a lookup on a path that is already
    /// formatting a string.
    #[must_use]
    pub fn kind_label(&self, kind: KindId) -> String {
        self.kind_name(kind).unwrap_or_else(|| kind.to_string())
    }

    /// The descriptor stored for a given kind id, or `None` if the id
    /// isn't registered. Returned as an owned clone from a published view.
    pub fn kind_descriptor(&self, kind: KindId) -> Option<KindDescriptor> {
        self.kinds.load().table().kinds.get(&kind).map(|slot| slot.descriptor.clone())
    }

    /// Snapshot of every kind descriptor currently registered. Sorted
    /// by name so the hub sees a deterministic ordering (ids are a
    /// hash of declaration-time data, so sorting on id would scramble
    /// unrelated kinds; name order preserves a human-readable grouping).
    /// Used by the control plane to ship an authoritative view to the
    /// hub after a runtime load or replace (ADR-0010 §4).
    pub fn list_kind_descriptors(&self) -> Vec<KindDescriptor> {
        let kinds = self.kinds.load();
        let mut out: Vec<KindDescriptor> = kinds.table().kinds.values().map(|slot| slot.descriptor.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}
