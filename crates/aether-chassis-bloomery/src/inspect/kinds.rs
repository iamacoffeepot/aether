//! The inspect actor's four wire kinds and the bounds every reply keeps.
//!
//! Every kind here is serde and crosses the wire, so `send_mail` reaches the
//! actor from the hub: a digest is `{"$hex": "<64 hex digits>"}`, and each
//! decoded value comes back as JSON text in a `String`, since no static schema
//! describes an arbitrary stored value.

use aether_actor::HeldReply;

/// The deepest `depth` an [`InspectArtifact`] may ask for; a deeper request
/// is refused `Err`.
pub const MAX_DEPTH: u32 = 4;

/// The most cited artifacts one [`InspectArtifact`] resolves inline. The
/// digests past it stay hex and the reply is `truncated`.
pub const MAX_ARTIFACTS: usize = 256;

/// The most JSON values one reply decodes, across every value it carries. A
/// value that would pass it stays hex (an artifact) or ends the page (events).
pub const MAX_VALUES: usize = 65_536;

/// The most payload bytes one hex rendering carries; its `length` still
/// names the whole payload.
pub const MAX_HEX_BYTES: usize = 4_096;

/// The most journal entries one [`InspectEvents`] reads, matched or not. The
/// caller pages on from `next_after`.
pub const MAX_SCANNED: usize = 4_096;

/// Read one stored artifact as JSON, resolving the artifacts it cites inline
/// down to `depth` levels.
#[aether_data::kind(name = "aether.bloomery.inspect.artifact", copy, eq)]
pub struct InspectArtifact {
    /// The artifact's digest.
    pub digest: [u8; 32],
    /// How many levels of cited artifacts to resolve inline, at most
    /// [`MAX_DEPTH`]. Zero renders every cited digest as hex.
    pub depth: u32,
}

/// The answer to one [`InspectArtifact`].
#[aether_data::kind(name = "aether.bloomery.inspect.artifact_result", eq)]
pub enum InspectArtifactResult {
    /// The artifact, decoded.
    Found {
        /// The artifact's stored kind id.
        kind_id: u64,
        /// The kind's name, when a schema source knows the kind.
        kind: Option<String>,
        /// The value as JSON text. Every 32-byte digest reads as lowercase
        /// hex, or, resolved, as `{digest, kind, value}`. A kind no schema
        /// source knows reads as `{kind_id, length, hex}`.
        json: String,
        /// Whether a bound cut the resolution short.
        truncated: bool,
    },
    /// No artifact is stored at the digest.
    Missing {
        /// The requested digest.
        digest: [u8; 32],
    },
    /// The request was refused, or the journal failed to read the artifact.
    Err {
        /// Human-readable failure.
        message: String,
    },
}

impl HeldReply for InspectArtifactResult {
    fn unanswered() -> Self {
        Self::Err { message: String::from("bloomery inspect closed before answering") }
    }
}

/// Read journal entries after `after` with their values decoded.
#[aether_data::kind(name = "aether.bloomery.inspect.events", eq)]
pub struct InspectEvents {
    /// Exclusive sequence boundary; zero begins at the first entry.
    pub after: u64,
    /// The most entries to answer, capped at the journal's page size (128).
    pub limit: u32,
    /// The kind names to keep. Empty keeps every kind.
    pub kinds: Vec<String>,
}

/// The answer to one [`InspectEvents`].
#[aether_data::kind(name = "aether.bloomery.inspect.events_result", eq)]
pub enum InspectEventsResult {
    /// The matching entries, in sequence order.
    Ok {
        /// The journal head the last page read saw.
        head: u64,
        /// The last sequence scanned, matched or not: the `after` of the next
        /// page. It equals the request's `after` when nothing was scanned.
        next_after: u64,
        /// The matching entries.
        events: Vec<InspectedEvent>,
    },
    /// The request was refused, or the journal failed to read a page.
    Err {
        /// Human-readable failure.
        message: String,
    },
}

impl HeldReply for InspectEventsResult {
    fn unanswered() -> Self {
        Self::Err { message: String::from("bloomery inspect closed before answering") }
    }
}

/// One journal entry with its value decoded.
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema, serde::Serialize, serde::Deserialize)]
pub struct InspectedEvent {
    /// Dense sequence assigned by the journal.
    pub seq: u64,
    /// The sequence this entry reacts to, if any.
    pub cause: Option<u64>,
    /// The entry's stored kind id.
    pub kind_id: u64,
    /// The kind's name, when a schema source knows the kind.
    pub kind: Option<String>,
    /// Journal time at insert, in unix milliseconds.
    pub recorded_at_millis: u64,
    /// The value as JSON text, every cited digest as lowercase hex.
    pub value: String,
}
