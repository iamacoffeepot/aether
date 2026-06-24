//! Manifest projection helpers for `aether.inventory`.
//!
//! Projects link-time [`ParamKind`] values onto their wire mirror for
//! the `aether.inventory.manifest` reply (ADR-0088 §4). The `#[actor]
//! impl` in `mod.rs` delegates here.

use aether_data::name_inventory::ParamKind;
use aether_kinds::ParamKindWire;

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
