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

/// `aether.component.republish_member` — the context a republish's prepare,
/// commit and abort carry into their answers (ADR-0243 §9): which republish,
/// and which of its members, the answer is for.
#[aether_data::kind(name = "aether.component.republish_member", copy, no_serde)]
pub struct RepublishMember {
    pub republish: u64,
    pub member: u32,
}

/// `aether.component.republish_published` — the context a republish's module
/// publish carries into its completion (ADR-0243 §9): the id of the
/// republish, whose held reply and members wait in host state under it.
#[aether_data::kind(name = "aether.component.republish_published", copy, no_serde)]
pub struct RepublishPublished {
    pub republish: u64,
}
