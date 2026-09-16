//! Entry envelope: identity, kind name, optional cause, wall clock, payload bytes.

use std::fmt;

/// Dense sequence number assigned by the store. Starts at 1; `Seq(0)` is the empty-journal head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub u64);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One recorded journal entry. `bytes` are the verbatim [`aether_data::Storage::encode_storage`] output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Store-assigned identity and fence.
    pub seq: Seq,
    /// `K::NAME` of the appended kind.
    pub kind: String,
    /// The `seq` this entry reacts to, if any.
    pub cause: Option<Seq>,
    /// Wall clock at insert, for people and consoles. A fold never reads it.
    pub recorded_at_millis: u64,
    /// Encoded payload, stored verbatim.
    pub bytes: Vec<u8>,
}
