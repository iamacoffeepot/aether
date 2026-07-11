//! Recipient-free player protocol frames.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::{KindId, MailboxId};
use serde::{Deserialize, Serialize};

/// Player wire version. Bump on any breaking change to [`PlayerFrame`].
pub const WIRE_VERSION: u32 = 1;

/// One recipient-free frame exchanged by a player client and session actor.
///
/// Intent and fact frames carry only a kind id and that kind's encoded
/// payload. The server chooses every recipient locally; no frame can address
/// an actor mailbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerFrame {
    /// Client-to-server handshake and declared self-identification.
    Hello { wire_version: u32, client_name: String },
    /// Server-assigned identity and authoritative clock watermark.
    HelloAck { wire_version: u32, session_identity: MailboxId, tick: u64, interval_nanos: u64 },
    /// Client-to-server intent. The session actor applies the closed allowlist.
    Intent { kind: KindId, payload: Vec<u8> },
    /// Server-to-client authoritative fact.
    Fact { kind: KindId, payload: Vec<u8> },
    /// Transport-pacing clock sample paired with one completed fact bundle.
    Beacon { tick: u64, server_nanos: u64, interval_nanos: u64 },
    /// Graceful protocol close.
    Close { reason: String },
}
