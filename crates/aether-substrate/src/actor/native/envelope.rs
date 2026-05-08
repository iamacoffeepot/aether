//! The owned-bytes envelope shape native dispatchers receive on their
//! mpsc inbox. Sinks today take borrowed args (`&str`, `&[u8]`); routing
//! across an mpsc channel forces ownership.

use crate::mail::{KindId, ReplyTo};

/// One mail delivered to a capability through its mpsc receiver.
///
/// Sinks today receive borrowed args (`&str`, `&[u8]`); routing across
/// an mpsc channel forces ownership. Capabilities that care about
/// ergonomics destructure this once at the top of their loop.
#[derive(Debug)]
pub struct Envelope {
    pub kind: KindId,
    pub kind_name: String,
    pub origin: Option<String>,
    pub sender: ReplyTo,
    pub payload: Vec<u8>,
    pub count: u32,
}
