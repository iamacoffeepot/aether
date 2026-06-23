// Wire descriptors for the substrate's kinds. Consumed by the native
// substrate binary and shipped to the hub at `Hello` per ADR-0007 so
// the hub can encode agent-supplied params for each kind.
//
// ADR-0019 PR 5: every substrate kind, including the control-plane
// vocabulary, ships as `KindEncoding::Schema(T::schema())`. There are
// no `Opaque` kinds left in the substrate's descriptor list — every
// kind is hub-encodable from agent params, and the `payload_bytes`
// escape hatch has been removed from the MCP `send_mail` tool.
//
// Issue #243: the descriptor list used to live as a manual
// `vec![schema::<Tick>(), schema::<Key>(), ...]` here. Adding a
// kind required a second touch to update this list, easy to forget
// — the safety net was a runtime "unknown kind" error at first send.
// Now the `Kind` derive macro emits a `cfg(not(target_arch = "wasm32"))`
// -gated `inventory::submit!` per type (paired with the existing wasm
// `aether.kinds` custom-section path); `all()` materializes the Hub-
// shipped `KindDescriptor` list by iterating the inventory slot.
// Adding a kind is one place — the struct definition with its derives.

// `all()` and its tests are native-only — the function materializes a
// Hub-shipped descriptor list from the inventory slot the Kind derive
// populates on non-wasm builds. wasm guests don't call it (their kind
// discovery rides the `aether.kinds` custom section, ADR-0032), and
// the inventory crate doesn't link on wasm32-unknown-unknown anyway.
#![cfg(not(target_arch = "wasm32"))]

use alloc::string::ToString;
use alloc::vec::Vec;

use aether_data::__inventory::DescriptorEntry;
use aether_data::KindDescriptor;

/// Every kind the substrate exposes. Order is unspecified — names are
/// the contract; downstream callers (`Registry::register_kind_with_descriptor`,
/// hub `Hello` handshake) are order-independent.
#[must_use]
pub fn all() -> Vec<KindDescriptor> {
    inventory::iter::<DescriptorEntry>()
        .map(|e| KindDescriptor {
            name: e.name.to_string(),
            schema: e.schema.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_list_is_unique() {
        // Inventory submission has no built-in dedup. Two declarations
        // of the same kind name would land here as duplicate entries —
        // probably a bug somewhere upstream of the registry.
        let descs = all();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names.len(),
            sorted.len(),
            "duplicate kind names in descriptors::all(): {names:?}",
        );
    }
}
