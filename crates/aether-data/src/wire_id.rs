//! Wire-side identity types: `EngineId`, `SessionToken`, plus a
//! re-export of `uuid::Uuid` so consumers don't have to add their own
//! `uuid` dep.
//!
//! These were defined in `aether-hub-protocol` until ADR-0071 phase 7c
//! moved them here. The hub channel still ships them on the wire, so
//! `aether-hub-protocol` re-exports — anything that only needs the
//! newtypes (substrate-core's `ReplyTarget`, the reply-table, the
//! egress backend trait) reaches for `aether_data::EngineId` etc.
//! without pulling in the framing crate.
//!
//! Both newtypes are `pub` tuple structs over `Uuid` so existing call
//! sites that match `EngineId(uuid)` keep working unchanged.

use serde::{Deserialize, Serialize};

pub use uuid::Uuid;

/// Hub-assigned stable identity for an engine connection. Fresh per
/// connect; not preserved across reconnects (resume-with-id is a V1
/// concern per ADR-0006).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EngineId(pub Uuid);

/// Hub-minted routing handle for a Claude MCP session. The engine
/// treats it as opaque bytes: it only echoes tokens the hub handed it
/// on inbound mail back as the address on a reply. The hub validates
/// on receipt; unknown/expired tokens produce an undeliverable status
/// (per ADR-0008).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionToken(pub Uuid);

impl SessionToken {
    /// Placeholder used before session tracking lands at the hub.
    /// Always treated as expired by the hub's validator.
    pub const NIL: SessionToken = SessionToken(Uuid::nil());
}
