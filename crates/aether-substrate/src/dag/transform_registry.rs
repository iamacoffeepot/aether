//! ADR-0048 §2 native-transform registry (iamacoffeepot/aether#1012).
//!
//! Native transforms are collected at link time into the inventory the
//! `#[transform]` macro populates (iamacoffeepot/aether#979). The
//! substrate builds one [`TransformRegistry`] at startup by iterating
//! [`aether_data::transforms()`]; the set is fixed for the process
//! lifetime — a transform set is a build-time property, not a load-time
//! one.
//!
//! The validator's dispatchability phase cross-checks each
//! `Transform { transform_id, output_kind_id }` node against this
//! registry (unknown id → [`DagError::UnknownTransform`], output-kind
//! mismatch → [`DagError::TransformOutputMismatch`]). Post-validation
//! lookup is an infallible hashmap hit.

use std::collections::HashMap;

use aether_data::{KindId, TransformEntry, TransformId};

/// A registered native transform's static metadata + invocation thunk
/// (ADR-0048 §2). A thin borrow over the link-time
/// [`TransformEntry`](aether_data::TransformEntry) — every field is
/// `'static`.
#[derive(Copy, Clone)]
pub struct RegisteredTransform {
    /// Declared input kind ids, in slot order (≤ 8).
    pub input_kind_ids: &'static [KindId],
    /// Declared output kind id.
    pub output_kind_id: KindId,
    /// `"{crate}::{module}::{fn}"` — diagnostics + MCP introspection.
    pub name: &'static str,
    /// Type-erased decode → call → encode thunk (ADR-0048 §1).
    pub invoke: aether_data::InvokeFn,
}

/// Substrate-global native-transform registry (ADR-0048 §2). Built once
/// at startup from the link-time inventory; immutable thereafter.
#[derive(Default)]
pub struct TransformRegistry {
    by_id: HashMap<TransformId, RegisteredTransform>,
}

impl TransformRegistry {
    /// Materialize the registry from the link-time inventory
    /// ([`aether_data::transforms()`]). A duplicate `transform_id`
    /// (two transforms hashing to the same name — practically
    /// impossible, but possible if two crates declare a transform with
    /// the same fully-qualified path) keeps the first and warns.
    #[must_use]
    pub fn from_inventory() -> Self {
        let mut by_id: HashMap<TransformId, RegisteredTransform> = HashMap::new();
        for entry in aether_data::transforms() {
            Self::insert_entry(&mut by_id, entry);
        }
        Self { by_id }
    }

    /// An empty registry — no native transforms. Used by the validator's
    /// tests and any chassis that doesn't host the executor.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    fn insert_entry(
        by_id: &mut HashMap<TransformId, RegisteredTransform>,
        entry: &'static TransformEntry,
    ) {
        let registered = RegisteredTransform {
            input_kind_ids: entry.input_kind_ids,
            output_kind_id: entry.output_kind_id,
            name: entry.name,
            invoke: entry.invoke,
        };
        if let Some(prior) = by_id.insert(entry.transform_id, registered) {
            tracing::warn!(
                target: "aether::dag::transform_registry",
                transform_id = %entry.transform_id,
                kept = prior.name,
                dropped = entry.name,
                "duplicate transform_id in link-time inventory; keeping the first",
            );
            // Restore the first-seen entry (HashMap::insert returned the
            // prior value, which we just clobbered).
            by_id.insert(entry.transform_id, prior);
        }
    }

    /// Resolve a transform by its global id. Post-validation this is an
    /// infallible hit (validation already rejected unknown ids).
    #[must_use]
    pub fn lookup(&self, id: TransformId) -> Option<RegisteredTransform> {
        self.by_id.get(&id).copied()
    }

    /// Number of registered transforms. Diagnostics / introspection.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// `true` when no native transforms are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Iterate `(transform_id, metadata)` for every registered
    /// transform, for MCP introspection (the transform listing
    /// ADR-0048 §2 names).
    pub fn iter(&self) -> impl Iterator<Item = (TransformId, RegisteredTransform)> + '_ {
        self.by_id.iter().map(|(id, t)| (*id, *t))
    }
}
