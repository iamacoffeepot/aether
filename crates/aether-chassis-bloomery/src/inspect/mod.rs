//! `aether.bloomery.inspect:<unit>`: reads any journal entry or artifact as
//! JSON, so a bloomery engine can be inspected over the hub with `send_mail`.
//!
//! The journal owner answers with storage-encoded bytes, and its artifact
//! reads are not serde, so nothing outside Rust could read them before. This
//! actor answers two serde kinds:
//!
//! - [`InspectArtifact`] returns one artifact as JSON, with the artifacts it
//!   cites resolved inline down to a bounded depth.
//! - [`InspectEvents`] returns journal entries after a sequence, filtered by
//!   kind name, each value decoded.
//!
//! A stored kind resolves to its schema through the text and bytes kinds, the
//! native storage-kind inventory, and then the program declarations the
//! driver answers, asked at most once per request. A kind no source knows
//! comes back as `{kind_id, length, hex}`, never as an error. The actor keeps
//! no schema or bundle table of its own, so it cannot go stale.
//!
//! The mount seam spawns it after the driver, under the unit key, over the
//! journal's and the driver's proven references.

mod artifact;
mod events;
mod kinds;
mod resolve;
mod runtime;

use aether_actor::actor;

pub use kinds::{
    InspectArtifact, InspectArtifactResult, InspectEvents, InspectEventsResult, InspectedEvent, MAX_ARTIFACTS,
    MAX_DEPTH, MAX_HEX_BYTES, MAX_SCANNED, MAX_VALUES,
};
pub use runtime::InspectParams;

/// `aether.bloomery.inspect` actor identity: one per unit, answering
/// [`InspectArtifact`] and [`InspectEvents`] from the unit's journal and its
/// driver's declarations.
#[actor(instanced, root)]
pub struct InspectActor;
