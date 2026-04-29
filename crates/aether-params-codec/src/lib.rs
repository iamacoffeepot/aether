//! Schema-driven encode + decode of agent-supplied params against
//! `aether_hub_protocol::SchemaType` descriptors. Pure functions —
//! no hub state, no async — so callers outside the supervisor (smoke
//! runner, future tooling) can use the same JSON ↔ wire-bytes path
//! the hub uses for `mcp__aether-hub__send_mail` and
//! `mcp__aether-hub__receive_mail`.
//!
//! Two wire shapes, picked per descriptor (ADR-0019, ADR-0020):
//!
//! 1. Cast-shaped (`Struct { repr_c: true }` and the recursive tree
//!    under it): `#[repr(C)]` byte layout. Decode is `bytemuck::cast`
//!    on the substrate side; encode walks the schema and writes the
//!    same layout.
//! 2. Postcard (everything else): postcard 1.x wire format, written
//!    and read directly to match the format byte-for-byte.

mod decode;
mod encode;

pub use decode::{DecodeError, decode_schema};
pub use encode::{EncodeError, encode_schema};
