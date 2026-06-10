//! Boot-time component autoload shared by the full-stack chassis
//! (iamacoffeepot/aether#1529, generalizing the #1520 desktop hook).
//!
//! A standalone bundle binary embeds an ordered component list and
//! populates its chassis env's `autoload` field; each chassis's
//! `build_inner` drains the list into `aether.component.load` mail
//! right after `.build()`, so the components come up with no hub. The
//! mail targets the generic `aether.component` mailbox — the same
//! address the hub's `load_component` and the test bench load through
//! — which is what makes the mechanism chassis-agnostic.

use aether_data::{Kind as _, mailbox_id_from_name};
use aether_kinds::LoadComponent;
use aether_substrate::Mail;

use crate::bundle_pack::PackedComponent;

/// A component to auto-load on boot — its wasm bytes, optional init-config
/// bytes (ADR-0090; empty for none), and the optional load name / export
/// selector that `aether.component.load` carries (ADR-0096). A standalone
/// bundle embeds these and feeds them to the chassis env's `autoload` list.
pub struct AutoloadComponent {
    pub wasm: Vec<u8>,
    pub config: Vec<u8>,
    pub name: Option<String>,
    pub export: Option<String>,
}

impl From<PackedComponent> for AutoloadComponent {
    fn from(packed: PackedComponent) -> Self {
        Self {
            wasm: packed.wasm,
            config: packed.config,
            name: packed.name,
            export: packed.export,
        }
    }
}

/// Build the `aether.component.load` mail that auto-loads `component`,
/// addressed to the `aether.component` mailbox the same way the hub's
/// `load_component` and the test bench do.
pub(crate) fn autoload_mail(component: AutoloadComponent) -> Mail {
    let payload = LoadComponent {
        wasm: component.wasm,
        name: component.name,
        config: component.config,
        export: component.export,
    }
    .encode_into_bytes();
    Mail::new(
        mailbox_id_from_name("aether.component"),
        LoadComponent::ID,
        payload,
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoload_mail_addresses_the_component_host() {
        // The autoload mail must target the component host's mailbox with the
        // load kind — the same address the hub and test bench load through.
        let mail = autoload_mail(AutoloadComponent {
            wasm: vec![0, 1, 2, 3],
            config: Vec::new(),
            name: Some("loco-motion".to_owned()),
            export: None,
        });
        assert_eq!(mail.recipient, mailbox_id_from_name("aether.component"));
        assert_eq!(mail.kind, LoadComponent::ID);
    }
}
