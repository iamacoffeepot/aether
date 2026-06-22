//! Manifest projection helpers for `aether.inventory`.
//!
//! Projects link-time [`ParamKind`] and [`Cardinality`] values onto
//! their wire mirrors for the `aether.inventory.manifest` reply
//! (ADR-0088 §4 v2). The `#[actor] impl` in `mod.rs` delegates here.

use aether_data::name_inventory::{Cardinality, ParamKind};
use aether_kinds::{CardinalityWire, ParamKindWire};

/// Project one link-time `ParamKind` onto its wire mirror. `Bounded`
/// / `Declared` carry their range / domain so the client expands the
/// family locally; `Dynamic` carries only the shape (its instances
/// reverse via [`Resolve`](aether_kinds::Resolve)).
pub fn param_kind_wire(param: &ParamKind) -> ParamKindWire {
    match *param {
        ParamKind::Bounded { lo, hi } => ParamKindWire::Bounded { lo, hi },
        ParamKind::Declared { domain } => ParamKindWire::Declared {
            domain: domain.to_vec(),
        },
        ParamKind::Dynamic => ParamKindWire::Dynamic,
    }
}

/// Project one link-time `Cardinality` onto its wire mirror (ADR-0088
/// §4 v2) — the orthogonal how-many axis the client surfaces verbatim.
pub fn cardinality_wire(cardinality: &Cardinality) -> CardinalityWire {
    match *cardinality {
        Cardinality::Bounded(count) => CardinalityWire::Bounded { count },
        Cardinality::OnePer(entity) => CardinalityWire::OnePer {
            entity: entity.into(),
        },
        Cardinality::Unbounded => CardinalityWire::Unbounded,
    }
}
