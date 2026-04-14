// Wire types for the engine ↔ hub protocol. Direction is enforced by
// the top-level enums `EngineToHub` / `HubToEngine`; the bodies are
// plain structs so they're ergonomic to construct and pattern-match.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Hub-assigned stable identity for an engine connection. Fresh per
/// connect; not preserved across reconnects (resume-with-id is a V1
/// concern per ADR-0006).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EngineId(pub Uuid);

/// First frame the engine sends after the TCP connection is open.
/// The hub replies with a `Welcome` carrying the assigned `EngineId`.
///
/// `kinds` declares every mail kind this engine's registry knows about
/// along with enough structural detail for the hub to encode agent-
/// supplied params for that kind (ADR-0007). Engines that don't want
/// schema-driven encoding can send an empty `kinds` and only the raw
/// `payload_bytes` path will work for their clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub pid: u32,
    pub started_unix: u64,
    pub version: String,
    pub kinds: Vec<KindDescriptor>,
}

/// One entry in `Hello.kinds`: a kind-name plus a wire-describable
/// encoding. Per ADR-0007 the hub uses these to encode agent-supplied
/// params into the exact bytes the engine expects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindDescriptor {
    pub name: String,
    pub encoding: KindEncoding,
}

/// How the hub can materialize bytes for a given kind.
///
/// - `Signal`: empty payload. `params` must be absent or empty.
/// - `Pod`: `#[repr(C)]` struct; hub encodes an ordered field list of
///   scalars and fixed-size scalar arrays matching Rust's layout.
/// - `Opaque`: the hub can't encode this from params. Clients must
///   supply raw bytes. V0 structural (postcard) kinds land here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KindEncoding {
    Signal,
    Pod { fields: Vec<PodField> },
    Opaque,
}

/// One field in a POD kind. Field order matches the Rust struct's
/// declaration order; the hub walks the list writing bytes with the
/// same alignment/padding the `#[repr(C)]` layout implies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodField {
    pub name: String,
    pub ty: PodFieldType,
}

/// A POD field is either a scalar or a fixed-length array of one
/// scalar primitive. Nested structs are deliberately out of scope for
/// V0 (ADR-0007) — a kind with nested-struct fields uses
/// `KindEncoding::Opaque` and the `payload_bytes` escape hatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PodFieldType {
    Scalar(PodPrimitive),
    Array { element: PodPrimitive, len: u32 },
}

/// POD primitive types the hub can encode. Matches the Rust primitive
/// set that's trivially `bytemuck::Pod`. Names are the canonical Rust
/// spelling so tooling can parse them without a lookup.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PodPrimitive {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

/// Hub's reply to `Hello`. Carries the `EngineId` the engine should
/// treat as its identity for the rest of this connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub engine_id: EngineId,
}

/// A piece of mail routed by the hub to an engine. Kind and recipient
/// are carried by name; the engine resolves them against its local
/// registry (per ADR-0005's kind registry).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFrame {
    pub recipient_name: String,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub count: u32,
}

/// Optional clean-shutdown marker. Either side may send it; receipt is
/// a signal that a subsequent TCP close is intentional rather than a
/// crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goodbye {
    pub reason: String,
}

/// Frames an engine sends to the hub. V0 omits engine-originated mail
/// and replies — those land when pub-sub lands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineToHub {
    Hello(Hello),
    Heartbeat,
    Goodbye(Goodbye),
}

/// Frames the hub sends to an engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HubToEngine {
    Welcome(Welcome),
    Heartbeat,
    Mail(MailFrame),
    Goodbye(Goodbye),
}
