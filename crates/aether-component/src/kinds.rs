//! Kinds the `aether.component` capability owns for its own internal mail.

/// `aether.component.boot_teardown` — the component host tears down a
/// module's boot trampoline (ADR-0147).
///
/// The host sends it through the boot trampoline's proven reference when the
/// module's last non-boot actor unloads, and the trampoline unloads its guest
/// as a drop does. It carries nothing because the recipient is the target,
/// and there is no reply.
#[aether_data::kind(name = "aether.component.boot_teardown", default, no_serde)]
pub struct BootTeardown {}

/// `aether.component.load_published` — the context a load's module publish
/// carries into its completion (ADR-0243 §9): the id of the load, whose held
/// reply and prepared inputs wait in host state under it.
#[aether_data::kind(name = "aether.component.load_published", copy, no_serde)]
pub struct LoadPublished {
    pub load: u64,
}

/// `aether.component.guest_born` — the context a staged guest birth carries
/// into its completion (ADR-0243 §9): which leg of a load the birth is, keyed
/// by what waits for it in host state.
#[aether_data::kind(name = "aether.component.guest_born", no_serde)]
pub enum GuestBorn {
    /// A module's boot guest, keyed by the module's content hash.
    Boot { hash: [u8; 32] },
    /// A load's requested guest, keyed by its load id.
    Requested { load: u64 },
}
