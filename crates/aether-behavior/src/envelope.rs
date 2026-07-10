//! The host<->script filter envelope (ADR-0137).
//!
//! A script's `filter` call returns one [`FilterOutput`]: a [`Verdict`] on
//! the in-flight mail plus an ordered list of [`Effect`]s the host drains
//! into real cluster sends. The verdict comes first and the effects apply
//! in recorded order, so stacked behaviors see each other's forwards and
//! never each other's in-flight effects.
//!
//! The output is a plain serde struct encoded with the `no_std`
//! `aether_data::wire` codec behind a leading [`ENVELOPE_VERSION`] byte.
//! Leaning on the shared wire helper keeps the encoding canonical and the
//! round-trip symmetric — which is why it earns no round-trip test (a
//! symmetric serde round-trip only fails if the codec is broken, and the
//! codec is tested where it lives).

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::wire;
use serde::{Deserialize, Serialize};

/// Leading byte on every encoded [`FilterOutput`]. Bumped when the wire
/// shape changes so a host decoding an older/newer script's output fails
/// loudly rather than misreading it.
pub const ENVELOPE_VERSION: u8 = 1;

/// The verdict a script returns on the in-flight mail. Mutation *is* the
/// verdict in author code (`&mut K` intercepts, `&K` observes); this is
/// the wire form the host drains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// The mail forwards. The bytes are the inbound kind's payload —
    /// re-encoded when a `&mut K` handler mutated it, the original bytes
    /// otherwise.
    Forward(Vec<u8>),
    /// The mail is dropped (`ctx.consume()`); it does not forward.
    Consume,
}

/// Where a drained [`Effect`] is delivered. Subname resolution
/// (subname -> cluster address) is the host's job at drain time; a script
/// only ever names a target relative to its own position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectTarget {
    /// The wrapped widget the host interposes on (`ctx.widget()`).
    Widget,
    /// A named child in the host's subtree (`ctx.child(path)`).
    Child(String),
    /// The parent lane (`ctx.panel()`).
    Panel,
}

/// One method-projected effect the host drains after the verdict applies:
/// a kind (`kind_id`) plus its encoded `bytes`, delivered to `target`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    /// Where the effect is delivered.
    pub target: EffectTarget,
    /// The effect kind's id (raw `u64`, as `KindId` carries no serde impl).
    pub kind_id: u64,
    /// The effect kind's encoded payload.
    pub bytes: Vec<u8>,
}

/// A script's whole filter output: a [`Verdict`] on the in-flight mail
/// and the ordered [`Effect`]s the host drains after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterOutput {
    /// The verdict on the in-flight mail (applied first).
    pub verdict: Verdict,
    /// The effects, in the order the script recorded them.
    pub effects: Vec<Effect>,
}

/// Encode a [`FilterOutput`] to the wire: an [`ENVELOPE_VERSION`] byte then
/// the `aether_data::wire` body.
///
/// # Panics
/// Only if the encoded body exceeds the wire codec's `u32` length ceiling —
/// unreachable for a well-formed filter output at any realistic effect count.
#[must_use]
pub fn encode(output: &FilterOutput) -> Vec<u8> {
    let body = wire::to_vec(output).expect("wire encode of FilterOutput fails only past the u32 ceiling");
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(ENVELOPE_VERSION);
    out.extend_from_slice(&body);
    out
}

/// Decode a [`FilterOutput`] the host drains from a script's `filter`
/// return. Returns `None` on an empty buffer, an unrecognized version
/// byte, or a malformed body.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<FilterOutput> {
    let (&version, body) = bytes.split_first()?;
    if version != ENVELOPE_VERSION {
        return None;
    }
    wire::from_bytes(body).ok()
}
