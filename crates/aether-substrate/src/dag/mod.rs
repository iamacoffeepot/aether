//! ADR-0047 computation-DAG runtime (substrate side).
//!
//! The wire vocabulary — `DagDescriptor`, `Node`, `Edge`, the
//! `aether.dag.{submit,cancel,status}` request kinds, the `Bundle`
//! meta-type, and the structured [`DagError`](aether_kinds::DagError)
//! set — lives in `aether-kinds::dag`. This module is the substrate-side
//! machinery that consumes it.
//!
//! Today that's the [`validator`] (iamacoffeepot/aether#975): the
//! three-phase submit-path check that turns a descriptor into a
//! topologically-sorted [`ValidatedDag`](validator::ValidatedDag) the
//! executor can dispatch from directly, or a structured
//! [`DagError`](aether_kinds::DagError) on the first rule violation; the
//! [`executor`] (iamacoffeepot/aether#976) that drives a validated DAG
//! to completion (source dispatch, observer parking, `Call`
//! collect-and-settle, cancellation, reaping); and its per-DAG
//! [`state`].

pub mod executor;
pub mod state;
pub mod transform_pool;
pub mod transform_registry;
pub mod validator;

use aether_data::SchemaType;
use aether_data::canonical::canonical_kind_bytes;

use crate::mail::{KindId, Registry};

/// Best-effort registered kind id for a schema: searches the registered
/// kind vocabulary for a descriptor whose schema canonically matches
/// `schema` and returns its id, falling back to `KindId(0)` when no
/// registered kind matches (the schema names a kind this substrate
/// doesn't know — still a usable answer, just without a precise id).
pub(crate) fn kind_id_for_schema(registry: &Registry, schema: &SchemaType) -> KindId {
    let target = canonical_kind_bytes("", schema);
    registry
        .list_kind_descriptors()
        .into_iter()
        .find(|d| canonical_kind_bytes("", &d.schema) == target)
        .map_or(KindId(0), |d| {
            registry.kind_id(&d.name).unwrap_or(KindId(0))
        })
}
