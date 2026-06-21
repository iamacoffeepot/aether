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
//! registry (unknown id → `DagError::UnknownTransform`, output-kind
//! mismatch → `DagError::TransformOutputMismatch`). Post-validation
//! lookup is an infallible hashmap hit.

use std::collections::HashMap;

use aether_data::{KindId, TransformEntry, TransformId};

/// Why a linear-fold chain fails validation (issue 2121). Returned by
/// [`TransformRegistry::validate_fold`] before any transform runs so
/// the caller gets a clean structural error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldError {
    /// No transform with this id is in the link-time inventory.
    UnknownTransform(TransformId),
    /// The transform at `at_index` has more than one input slot — it
    /// cannot sit in a linear fold where only one input is threaded.
    NonLinearArity { at_index: usize, arity: usize },
    /// The output kind of transform `at_index - 1` does not match the
    /// input kind of transform `at_index`.
    KindMismatch {
        at_index: usize,
        expected: KindId,
        found: KindId,
    },
}

/// A registered native transform's static metadata + invocation thunk
/// (ADR-0048 §2). A thin borrow over the link-time [`TransformEntry`] —
/// every field is `'static`.
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

    /// Validate that `chain` forms a composable linear fold (issue 2121).
    ///
    /// - Empty chain → `Ok(None)` (short-circuit; no kind anchoring
    ///   needed).
    /// - Non-empty chain → each id must resolve; each transform must
    ///   have exactly one input slot; each adjacent pair must compose
    ///   (the prior transform's `output_kind_id` == the next's
    ///   `input_kind_ids[0]`). Returns `Ok(Some(output_kind_id))` of
    ///   the last transform.
    ///
    /// Validation is a pure scan — no transform is invoked.
    pub fn validate_fold(&self, chain: &[TransformId]) -> Result<Option<KindId>, FoldError> {
        if chain.is_empty() {
            return Ok(None);
        }
        let mut prev_output: Option<KindId> = None;
        for (i, &id) in chain.iter().enumerate() {
            let t = self.lookup(id).ok_or(FoldError::UnknownTransform(id))?;
            if t.input_kind_ids.len() != 1 {
                return Err(FoldError::NonLinearArity {
                    at_index: i,
                    arity: t.input_kind_ids.len(),
                });
            }
            if let Some(expected) = prev_output {
                let found = t.input_kind_ids[0];
                if expected != found {
                    return Err(FoldError::KindMismatch {
                        at_index: i,
                        expected,
                        found,
                    });
                }
            }
            prev_output = Some(t.output_kind_id);
        }
        Ok(prev_output)
    }
}

#[cfg(test)]
mod tests {
    use aether_data::TransformError;

    use super::*;

    #[allow(clippy::unnecessary_wraps)]
    fn noop_invoke(_: &[&[u8]]) -> Result<Vec<u8>, TransformError> {
        Ok(vec![])
    }

    fn make_registry(entries: &[(TransformId, &'static [KindId], KindId)]) -> TransformRegistry {
        let mut reg = TransformRegistry::empty();
        for &(id, inputs, output) in entries {
            reg.by_id.insert(
                id,
                RegisteredTransform {
                    input_kind_ids: inputs,
                    output_kind_id: output,
                    name: "test::noop",
                    invoke: noop_invoke,
                },
            );
        }
        reg
    }

    const A_ID: TransformId = TransformId(0xBEEF_0001_0000_0001);
    const B_ID: TransformId = TransformId(0xBEEF_0001_0000_0002);
    const MULTI_ID: TransformId = TransformId(0xBEEF_0001_0000_0003);

    const K1: KindId = KindId(0x1111_1111_1111_0001);
    const K2: KindId = KindId(0x1111_1111_1111_0002);
    const K3: KindId = KindId(0x1111_1111_1111_0003);

    const A_INPUTS: &[KindId] = &[K1];
    const B_INPUTS: &[KindId] = &[K2];
    const B_WRONG_INPUTS: &[KindId] = &[K3];
    const MULTI_INPUTS: &[KindId] = &[K1, K2];

    #[test]
    fn empty_chain_returns_none() {
        let reg = make_registry(&[(A_ID, A_INPUTS, K2)]);
        assert_eq!(reg.validate_fold(&[]), Ok(None));
    }

    #[test]
    fn empty_chain_on_empty_registry_returns_none() {
        let reg = TransformRegistry::empty();
        assert_eq!(reg.validate_fold(&[]), Ok(None));
    }

    #[test]
    fn single_transform_returns_its_output_kind() {
        let reg = make_registry(&[(A_ID, A_INPUTS, K2)]);
        assert_eq!(reg.validate_fold(&[A_ID]), Ok(Some(K2)));
    }

    #[test]
    fn composed_pair_returns_final_output_kind() {
        let reg = make_registry(&[(A_ID, A_INPUTS, K2), (B_ID, B_INPUTS, K3)]);
        assert_eq!(reg.validate_fold(&[A_ID, B_ID]), Ok(Some(K3)));
    }

    #[test]
    fn unknown_id_returns_error() {
        let reg = TransformRegistry::empty();
        let bogus = TransformId(0xDEAD_DEAD_DEAD_DEAD);
        assert_eq!(
            reg.validate_fold(&[bogus]),
            Err(FoldError::UnknownTransform(bogus)),
        );
    }

    #[test]
    fn arity_gt_one_returns_non_linear_arity() {
        let reg = make_registry(&[(MULTI_ID, MULTI_INPUTS, K3)]);
        assert_eq!(
            reg.validate_fold(&[MULTI_ID]),
            Err(FoldError::NonLinearArity {
                at_index: 0,
                arity: 2
            }),
        );
    }

    #[test]
    fn mismatched_pair_returns_kind_mismatch_at_index_one() {
        // A: K1 → K2; B_WRONG: K3 → K3. A's output (K2) ≠ B_WRONG's
        // input (K3) → KindMismatch at index 1.
        let reg = make_registry(&[(A_ID, A_INPUTS, K2), (B_ID, B_WRONG_INPUTS, K3)]);
        assert_eq!(
            reg.validate_fold(&[A_ID, B_ID]),
            Err(FoldError::KindMismatch {
                at_index: 1,
                expected: K2,
                found: K3,
            }),
        );
    }
}
