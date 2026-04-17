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
/// - `Schema`: ADR-0019 unified vocabulary covering scalars, strings,
///   vecs, options, enums, and nested structs. The `repr_c` flag on
///   `SchemaType::Struct` opts a struct into the cast-shaped wire
///   format (today's `Pod` bytes); everything else is postcard.
///
/// `Signal`/`Pod`/`Opaque` are kept here through the migration so
/// `aether-hub`, `aether-substrate`, and the smoke binaries can move
/// over one PR at a time. The cleanup PR deletes them along with
/// `PodField`/`PodFieldType`/`PodPrimitive`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KindEncoding {
    Signal,
    Pod { fields: Vec<PodField> },
    Opaque,
    Schema(SchemaType),
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

/// ADR-0019 schema type vocabulary. Describes the structure of a mail
/// kind's payload in enough detail for the hub to encode it from
/// agent-supplied params and the substrate to decode it into a typed
/// value. `Struct.repr_c = true` selects the cast-shaped wire format
/// (raw `#[repr(C)]` bytes); everything else is postcard.
///
/// Restrictions on `repr_c = true` (enforced by the SDK derive, not
/// the wire format): only legal when every field is itself
/// cast-eligible — `Scalar`, `Array` of cast-eligible elements, or a
/// nested `Struct { repr_c: true, .. }`. `String`, `Bytes`, `Vec`,
/// `Option`, and `Enum` fields disqualify a struct from `repr_c`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaType {
    Unit,
    Bool,
    Scalar(Primitive),
    String,
    Bytes,
    Option(Box<SchemaType>),
    Vec(Box<SchemaType>),
    Array {
        element: Box<SchemaType>,
        len: u32,
    },
    Struct {
        fields: Vec<NamedField>,
        repr_c: bool,
    },
    Enum {
        variants: Vec<EnumVariant>,
    },
}

/// One field inside a `SchemaType::Struct` or struct-shaped enum
/// variant. Field order matches the Rust source order; for cast-shaped
/// structs (`repr_c: true`) it also matches `#[repr(C)]` layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedField {
    pub name: String,
    pub ty: SchemaType,
}

/// One variant of a `SchemaType::Enum`. Discriminants are explicit
/// `u32`s so the wire encoding doesn't depend on declaration order —
/// adding a variant later (without renumbering existing ones) is
/// forward-compatible at the postcard level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnumVariant {
    Unit {
        name: String,
        discriminant: u32,
    },
    Tuple {
        name: String,
        discriminant: u32,
        fields: Vec<SchemaType>,
    },
    Struct {
        name: String,
        discriminant: u32,
        fields: Vec<NamedField>,
    },
}

/// Scalar primitives addressable by `SchemaType::Scalar`. Same set as
/// `PodPrimitive` (which is parallel for the legacy `Pod` arm during
/// migration); kept as its own type so `Schema` can outlive `Pod` once
/// the cleanup PR removes the legacy arm.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Primitive {
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
/// registry (per ADR-0005's kind registry). `sender` is the hub's
/// routing handle for the originating Claude session — components
/// that want to reply-to-sender echo it back on an outbound
/// `EngineMailFrame` (ADR-0008).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFrame {
    pub recipient_name: String,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub count: u32,
    pub sender: SessionToken,
}

/// A piece of mail the engine is sending to one or more Claude
/// sessions through the hub. The hub owns session routing, so the
/// engine addresses by `ClaudeAddress` rather than by session id or
/// recipient name (ADR-0008). `origin` is the substrate-local mailbox
/// name of the emitting component (ADR-0011); `None` for substrate
/// core pushes that have no sending mailbox. The hub forwards it
/// verbatim and does not validate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineMailFrame {
    pub address: ClaudeAddress,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub origin: Option<String>,
}

/// How an engine-originated mail is addressed at the hub. `Session`
/// targets the specific MCP session whose token the engine is echoing
/// from an earlier inbound mail; `Broadcast` fan-outs to every
/// currently attached session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeAddress {
    Session(SessionToken),
    Broadcast,
}

/// Optional clean-shutdown marker. Either side may send it; receipt is
/// a signal that a subsequent TCP close is intentional rather than a
/// crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goodbye {
    pub reason: String,
}

/// Frames an engine sends to the hub. `Mail` is the observation path
/// (ADR-0008): engine-originated mail addressed to a Claude session
/// or broadcast to all sessions. `KindsChanged` (ADR-0010 §4) tells
/// the hub to replace its cached descriptor list for this engine —
/// needed after `aether.control.load_component` /
/// `aether.control.replace_component` registers a new kind, which the
/// hub would otherwise miss since its cache is pinned at `Hello`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineToHub {
    Hello(Hello),
    Heartbeat,
    Mail(EngineMailFrame),
    KindsChanged(Vec<KindDescriptor>),
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
