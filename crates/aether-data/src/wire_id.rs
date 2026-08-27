//! Wire-side identity types: `EngineId`, `SessionToken`, plus a
//! re-export of `uuid::Uuid` so consumers don't have to add their own
//! `uuid` dep.
//!
//! These were defined in `aether-hub-protocol` until ADR-0071 phase 7c
//! moved them here. The hub channel still ships them on the wire, so
//! `aether-hub-protocol` re-exports — anything that only needs the
//! newtypes (substrate-core's `SourceAddr`, the reply-table, the
//! egress backend trait) reaches for `aether_data::EngineId` etc.
//! without pulling in the framing crate.
//!
//! Both newtypes are `pub` tuple structs over `Uuid` so existing call
//! sites that match `EngineId(uuid)` keep working unchanged.

use alloc::borrow::Cow;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::Schema;
use crate::schema::{LabelNode, NamedField, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode, decode_bytes, encode_bytes};

pub use uuid::Uuid;

/// Hub-assigned stable identity for an engine connection. Fresh per
/// connect; not preserved across reconnects (resume-with-id is a V1
/// concern per ADR-0006).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EngineId(pub Uuid);

impl Schema for EngineId {
    const SCHEMA: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[NamedField { name: Cow::Borrowed("uuid"), ty: SchemaType::Bytes }]),
        repr_c: false,
    };
    const LABEL: Option<&'static str> = Some("aether.engine_id");
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl WireEncode for EngineId {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        encode_bytes(out, self.0.as_bytes())
    }
}

impl<'de> WireDecode<'de> for EngineId {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let bytes = decode_bytes(cursor)?;
        let uuid_bytes: [u8; 16] = bytes.try_into().map_err(|_| WireError::Length)?;
        Ok(Self(Uuid::from_bytes(uuid_bytes)))
    }
}

/// Hub-minted routing handle for a Claude MCP session. The engine
/// treats it as opaque bytes: it only echoes tokens the hub handed it
/// on inbound mail back as the address on a reply. The hub validates
/// on receipt; unknown/expired tokens produce an undeliverable status
/// (per ADR-0008).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionToken(pub Uuid);

impl Schema for SessionToken {
    const SCHEMA: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[NamedField { name: Cow::Borrowed("uuid"), ty: SchemaType::Bytes }]),
        repr_c: false,
    };
    const LABEL: Option<&'static str> = Some("aether.session_token");
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl WireEncode for SessionToken {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        encode_bytes(out, self.0.as_bytes())
    }
}

impl<'de> WireDecode<'de> for SessionToken {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let bytes = decode_bytes(cursor)?;
        let uuid_bytes: [u8; 16] = bytes.try_into().map_err(|_| WireError::Length)?;
        Ok(Self(Uuid::from_bytes(uuid_bytes)))
    }
}

impl SessionToken {
    /// Placeholder used before session tracking lands at the hub.
    /// Always treated as expired by the hub's validator.
    pub const NIL: Self = Self(Uuid::nil());
}
