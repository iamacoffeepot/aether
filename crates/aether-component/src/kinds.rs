//! Kinds the `aether.component` capability owns for its own internal mail.

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
