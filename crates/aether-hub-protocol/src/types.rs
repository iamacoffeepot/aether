// Wire types for the engine ↔ hub protocol. Direction is enforced by
// the top-level enums `EngineToHub` / `HubToEngine`; the bodies are
// plain structs so they're ergonomic to construct and pattern-match.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

/// One entry in `Hello.kinds`: a kind-name plus its schema. The hub
/// uses the schema to encode agent-supplied params into the exact
/// bytes the engine expects (cast-shaped or postcard, ADR-0019).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindDescriptor {
    pub name: String,
    pub schema: SchemaType,
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
/// `Option`, `Map`, and `Enum` fields disqualify a struct from
/// `repr_c`.
///
/// ADR-0031: every recursive field uses `SchemaCell` (static-or-owned)
/// and every collection/string uses `Cow<'static, _>` so the whole type
/// is const-constructible. Derive(Schema) emits a single
/// `const SCHEMA: SchemaType = …` literal; the hub's deserializer
/// produces the `Owned` / `Cow::Owned` variants. Walkers Deref through
/// both without observing the difference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaType {
    Unit,
    Bool,
    Scalar(Primitive),
    String,
    Bytes,
    Option(SchemaCell),
    Vec(SchemaCell),
    Array {
        element: SchemaCell,
        len: u32,
    },
    Struct {
        fields: Cow<'static, [NamedField]>,
        repr_c: bool,
    },
    Enum {
        variants: Cow<'static, [EnumVariant]>,
    },
    /// ADR-0045 typed handle reference. A field whose type is
    /// `Ref<K>` accepts either an inline `K` value or a
    /// `Handle { id, kind_id }` pointing into the substrate's
    /// handle store. The cell wraps the inner kind's schema —
    /// recipients dispatch identically against the resolved
    /// inline value, so existing decoders only need to learn
    /// the new tag at the field-walk step.
    Ref(SchemaCell),
    /// Issue #232: keyed lookup table. Wire form is postcard's
    /// `BTreeMap<K, V>` — `varint(len) + (k, v)` pairs in
    /// key-sorted order. Key types are restricted to `String`,
    /// integer `Scalar`s, and `Bool` (proto3-style stringify
    /// rule); the Rust-level `BTreeMap<K: Ord, V>` bound makes
    /// `f32`/`f64`/`Vec`/`Option` unreachable at the type level
    /// and the codec rejects them defensively. Disqualifies a
    /// parent struct from `repr_c` — variable-length data has
    /// no fixed `#[repr(C)]` layout.
    Map {
        key: SchemaCell,
        value: SchemaCell,
    },
    /// ADR-0065: a first-class typed reference, identified by a
    /// 64-bit type id (FNV-1a of the type's canonical name with a
    /// disjoint `TYPE_DOMAIN` prefix). The codec's `TypeId` arm
    /// hard-codes the per-id encode/decode logic — for v1, the three
    /// known type ids are `MailboxId`, `KindId`, `HandleId`, all of
    /// which are u64 varint on postcard and tagged-string on JSON.
    /// Cast-shape size/align is 8 bytes, 8-byte align — same as a
    /// `u64`, so a typed-id field embedded in a `repr_c: true`
    /// struct keeps the parent's cast-eligibility.
    TypeId(u64),
}

/// Recursion-breaking indirection for nested `SchemaType` fields
/// (ADR-0031). `Static(&'static SchemaType)` is the const-literal arm —
/// derives and hand-rolled impls reference the nested type's
/// `<T as Schema>::SCHEMA` through this variant at compile time.
/// `Owned(Box<SchemaType>)` is the wire arm — the hub's postcard
/// decoder allocates one `Box` per recursive node. Both `Deref` to
/// `&SchemaType`, so walkers don't observe which variant carries the
/// value. `Cow<'static, SchemaType>` would infinite-size through its
/// `Owned(T)` arm; `SchemaCell` breaks the cycle via `Box`.
#[derive(Debug)]
pub enum SchemaCell {
    Static(&'static SchemaType),
    Owned(Box<SchemaType>),
}

impl SchemaCell {
    /// Construct an `Owned` cell from a schema value. Convenience for
    /// code paths that build schemas at runtime (mostly tests and the
    /// hub's decoder). Production const callers use `Static(&FOO)`.
    pub fn owned(schema: SchemaType) -> Self {
        SchemaCell::Owned(Box::new(schema))
    }
}

impl core::ops::Deref for SchemaCell {
    type Target = SchemaType;
    fn deref(&self) -> &SchemaType {
        match self {
            SchemaCell::Static(r) => r,
            SchemaCell::Owned(b) => b,
        }
    }
}

impl AsRef<SchemaType> for SchemaCell {
    fn as_ref(&self) -> &SchemaType {
        self
    }
}

impl Clone for SchemaCell {
    fn clone(&self) -> Self {
        // Clone normalizes to Owned so the clone doesn't require the
        // source to be 'static. A Static cell cloned from a const
        // literal lands as Owned(Box::new(copy_of_value)); the value
        // is identical, the variant chosen expresses "this clone has
        // its own allocation."
        SchemaCell::Owned(Box::new((**self).clone()))
    }
}

impl PartialEq for SchemaCell {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for SchemaCell {}

impl Serialize for SchemaCell {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (**self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SchemaCell {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SchemaType::deserialize(deserializer).map(SchemaCell::owned)
    }
}

/// One field inside a `SchemaType::Struct` or struct-shaped enum
/// variant. Field order matches the Rust source order; for cast-shaped
/// structs (`repr_c: true`) it also matches `#[repr(C)]` layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedField {
    pub name: Cow<'static, str>,
    pub ty: SchemaType,
}

/// One variant of a `SchemaType::Enum`. Discriminants are explicit
/// `u32`s so the wire encoding doesn't depend on declaration order —
/// adding a variant later (without renumbering existing ones) is
/// forward-compatible at the postcard level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnumVariant {
    Unit {
        name: Cow<'static, str>,
        discriminant: u32,
    },
    Tuple {
        name: Cow<'static, str>,
        discriminant: u32,
        fields: Cow<'static, [SchemaType]>,
    },
    Struct {
        name: Cow<'static, str>,
        discriminant: u32,
        fields: Cow<'static, [NamedField]>,
    },
}

impl EnumVariant {
    /// Variant's wire name — matches the `#[postcard(...)]` rename (if
    /// any) or the bare Rust variant identifier. Used on both the
    /// encode and decode sides for lookup and error reporting.
    pub fn name(&self) -> &str {
        match self {
            EnumVariant::Unit { name, .. }
            | EnumVariant::Tuple { name, .. }
            | EnumVariant::Struct { name, .. } => name,
        }
    }

    /// Postcard discriminant — the varint written on the wire before
    /// the variant body. Assigned by the derive at schema-build time
    /// and stable for the life of the kind vocabulary.
    pub fn discriminant(&self) -> u32 {
        match self {
            EnumVariant::Unit { discriminant, .. }
            | EnumVariant::Tuple { discriminant, .. }
            | EnumVariant::Struct { discriminant, .. } => *discriminant,
        }
    }
}

/// Scalar primitives addressable by `SchemaType::Scalar`. Matches the
/// Rust primitive set that's trivially `bytemuck::Pod` so cast-shaped
/// structs can express their leaf types; `bool` is `SchemaType::Bool`,
/// not a `Primitive` (the cast path doesn't accept `bool` fields).
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

/// Positional-only twin of `SchemaType`: same variant arms, same field
/// ordering, but with every name field stripped (ADR-0032). This is the
/// wire shape of the hashed `aether.kinds` manifest section and the
/// input that `fnv1a_64_prefixed` chews on (after the `KIND_DOMAIN`
/// prefix) to produce `Kind::ID`. Postcard-
/// compatible with `SchemaType` at the subset of bytes they share — the
/// canonical serializer emits bytes that deserialize cleanly into
/// `SchemaShape` via `postcard::from_bytes`.
///
/// Not const-constructible. Lives purely on the decode side of the
/// wire: hub parses manifest bytes into `SchemaShape`, then merges
/// with a parallel `LabelNode` from the labels sidecar to reconstruct
/// a named `SchemaType` for its encoder/decoder paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaShape {
    Unit,
    Bool,
    Scalar(Primitive),
    String,
    Bytes,
    Option(Box<SchemaShape>),
    Vec(Box<SchemaShape>),
    Array {
        element: Box<SchemaShape>,
        len: u32,
    },
    Struct {
        fields: Vec<SchemaShape>,
        repr_c: bool,
    },
    Enum {
        variants: Vec<VariantShape>,
    },
    Ref(Box<SchemaShape>),
    Map {
        key: Box<SchemaShape>,
        value: Box<SchemaShape>,
    },
    /// ADR-0065 first-class typed reference. Wire-identical to
    /// `SchemaType::TypeId(u64)`.
    TypeId(u64),
}

/// Positional enum variant — `VariantShape::Tuple { discriminant, fields }`
/// lines up with `EnumVariant::Tuple { name, discriminant, fields }` via
/// the canonical serializer dropping `name`. Same reasoning for the other
/// arms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VariantShape {
    Unit {
        discriminant: u32,
    },
    Tuple {
        discriminant: u32,
        fields: Vec<SchemaShape>,
    },
    Struct {
        discriminant: u32,
        fields: Vec<SchemaShape>,
    },
}

/// Kind-level canonical record — the name-plus-positional-schema pair
/// the `aether.kinds` section carries (ADR-0032). Postcard-compatible
/// with `KindDescriptor` at the `name` field (both serialize as
/// length-prefixed UTF-8), and with the canonical schema bytes at the
/// `schema` field. The hub decodes one `KindShape` per section record
/// via `postcard::take_from_bytes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindShape {
    pub name: Cow<'static, str>,
    pub schema: SchemaShape,
}

/// Labels sidecar — parallel-shape tree of nominal information
/// (ADR-0032). Required at the hub; the canonical schema carries no
/// names, so the hub needs the labels section to map MCP JSON params
/// to postcard positions and to render `describe_kinds` output.
///
/// Arms mirror `SchemaType`'s structure so a walker can step both
/// trees in lockstep. `Anonymous` covers primitives, `String`, and
/// `Bytes` — leaves with no nominal information to carry. Container
/// arms (`Option`, `Vec`, `Array`) carry a `LabelCell` wrapping the
/// element's labels; the container itself has no nominal info.
/// `Struct` and `Enum` carry the full Rust type label plus field /
/// variant names.
#[derive(Debug)]
pub enum LabelNode {
    /// Primitive, `String`, or `Bytes` — no nominal info to carry.
    /// Also used for struct/enum types whose author didn't set a
    /// `LABEL` (hand-rolled `Schema` impls that skip the label const).
    Anonymous,
    Option(LabelCell),
    Vec(LabelCell),
    Array(LabelCell),
    Struct {
        type_label: Option<Cow<'static, str>>,
        field_names: Cow<'static, [Cow<'static, str>]>,
        fields: Cow<'static, [LabelNode]>,
    },
    Enum {
        type_label: Option<Cow<'static, str>>,
        variants: Cow<'static, [VariantLabel]>,
    },
    Ref(LabelCell),
    /// Issue #232: parallel labels for `SchemaType::Map`. Both
    /// key and value carry their own cell because either side may
    /// be a struct/enum whose nominal info needs preserving for
    /// `describe_kinds` rendering.
    Map {
        key: LabelCell,
        value: LabelCell,
    },
}

/// Per-variant nominal info for `LabelNode::Enum`. `name` is the Rust
/// variant identifier (`"Pending"`, `"Ok"`, `"Err"`); tuple variant
/// fields stay positional labels-only (the schema side provides the
/// shape, labels just name the variant).
#[derive(Debug)]
pub enum VariantLabel {
    Unit {
        name: Cow<'static, str>,
    },
    Tuple {
        name: Cow<'static, str>,
        fields: Cow<'static, [LabelNode]>,
    },
    Struct {
        name: Cow<'static, str>,
        field_names: Cow<'static, [Cow<'static, str>]>,
        fields: Cow<'static, [LabelNode]>,
    },
}

/// Recursion-breaking cell for nested `LabelNode` fields, twin of
/// `SchemaCell`. Same rationale: `Static` for const literals (derive
/// emits `LabelCell::Static(&<T as Schema>::LABEL_NODE)`), `Owned`
/// for post-deserialize values.
#[derive(Debug)]
pub enum LabelCell {
    Static(&'static LabelNode),
    Owned(Box<LabelNode>),
}

impl LabelCell {
    pub fn owned(node: LabelNode) -> Self {
        LabelCell::Owned(Box::new(node))
    }
}

impl core::ops::Deref for LabelCell {
    type Target = LabelNode;
    fn deref(&self) -> &LabelNode {
        match self {
            LabelCell::Static(r) => r,
            LabelCell::Owned(b) => b,
        }
    }
}

impl AsRef<LabelNode> for LabelCell {
    fn as_ref(&self) -> &LabelNode {
        self
    }
}

impl Clone for LabelCell {
    fn clone(&self) -> Self {
        LabelCell::Owned(Box::new((**self).clone()))
    }
}

impl PartialEq for LabelCell {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for LabelCell {}

impl Serialize for LabelCell {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (**self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LabelCell {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        LabelNode::deserialize(deserializer).map(LabelCell::owned)
    }
}

impl Clone for LabelNode {
    fn clone(&self) -> Self {
        match self {
            LabelNode::Anonymous => LabelNode::Anonymous,
            LabelNode::Option(c) => LabelNode::Option(c.clone()),
            LabelNode::Vec(c) => LabelNode::Vec(c.clone()),
            LabelNode::Array(c) => LabelNode::Array(c.clone()),
            LabelNode::Struct {
                type_label,
                field_names,
                fields,
            } => LabelNode::Struct {
                type_label: type_label.clone(),
                field_names: field_names.clone(),
                fields: fields.clone(),
            },
            LabelNode::Enum {
                type_label,
                variants,
            } => LabelNode::Enum {
                type_label: type_label.clone(),
                variants: variants.clone(),
            },
            LabelNode::Ref(c) => LabelNode::Ref(c.clone()),
            LabelNode::Map { key, value } => LabelNode::Map {
                key: key.clone(),
                value: value.clone(),
            },
        }
    }
}

impl PartialEq for LabelNode {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (LabelNode::Anonymous, LabelNode::Anonymous) => true,
            (LabelNode::Option(a), LabelNode::Option(b)) => a == b,
            (LabelNode::Vec(a), LabelNode::Vec(b)) => a == b,
            (LabelNode::Array(a), LabelNode::Array(b)) => a == b,
            (
                LabelNode::Struct {
                    type_label: la,
                    field_names: na,
                    fields: fa,
                },
                LabelNode::Struct {
                    type_label: lb,
                    field_names: nb,
                    fields: fb,
                },
            ) => la == lb && na == nb && fa == fb,
            (
                LabelNode::Enum {
                    type_label: la,
                    variants: va,
                },
                LabelNode::Enum {
                    type_label: lb,
                    variants: vb,
                },
            ) => la == lb && va == vb,
            (LabelNode::Ref(a), LabelNode::Ref(b)) => a == b,
            (LabelNode::Map { key: ka, value: va }, LabelNode::Map { key: kb, value: vb }) => {
                ka == kb && va == vb
            }
            _ => false,
        }
    }
}

impl Eq for LabelNode {}

impl Serialize for LabelNode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Hand-rolled to match the wire shape of `#[derive(Serialize)]`
        // over the same variants — postcard treats each enum arm by
        // position + body.
        use serde::ser::SerializeStructVariant;
        use serde::ser::SerializeTupleVariant;
        match self {
            LabelNode::Anonymous => serializer.serialize_unit_variant("LabelNode", 0, "Anonymous"),
            LabelNode::Option(cell) => {
                let mut s = serializer.serialize_tuple_variant("LabelNode", 1, "Option", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            LabelNode::Vec(cell) => {
                let mut s = serializer.serialize_tuple_variant("LabelNode", 2, "Vec", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            LabelNode::Array(cell) => {
                let mut s = serializer.serialize_tuple_variant("LabelNode", 3, "Array", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            LabelNode::Struct {
                type_label,
                field_names,
                fields,
            } => {
                let mut s = serializer.serialize_struct_variant("LabelNode", 4, "Struct", 3)?;
                s.serialize_field("type_label", type_label)?;
                s.serialize_field("field_names", field_names)?;
                s.serialize_field("fields", fields)?;
                s.end()
            }
            LabelNode::Enum {
                type_label,
                variants,
            } => {
                let mut s = serializer.serialize_struct_variant("LabelNode", 5, "Enum", 2)?;
                s.serialize_field("type_label", type_label)?;
                s.serialize_field("variants", variants)?;
                s.end()
            }
            LabelNode::Ref(cell) => {
                let mut s = serializer.serialize_tuple_variant("LabelNode", 6, "Ref", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            LabelNode::Map { key, value } => {
                let mut s = serializer.serialize_struct_variant("LabelNode", 7, "Map", 2)?;
                s.serialize_field("key", key)?;
                s.serialize_field("value", value)?;
                s.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for LabelNode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Mirror `LabelNode::Serialize`'s variant tagging. Matches
        // postcard's enum encoding: varint discriminant, then body.
        #[derive(Serialize, Deserialize)]
        enum LabelNodeDe {
            Anonymous,
            Option(LabelCell),
            Vec(LabelCell),
            Array(LabelCell),
            Struct {
                type_label: Option<Cow<'static, str>>,
                field_names: Vec<Cow<'static, str>>,
                fields: Vec<LabelNode>,
            },
            Enum {
                type_label: Option<Cow<'static, str>>,
                variants: Vec<VariantLabel>,
            },
            Ref(LabelCell),
            Map {
                key: LabelCell,
                value: LabelCell,
            },
        }
        match LabelNodeDe::deserialize(deserializer)? {
            LabelNodeDe::Anonymous => Ok(LabelNode::Anonymous),
            LabelNodeDe::Option(c) => Ok(LabelNode::Option(c)),
            LabelNodeDe::Vec(c) => Ok(LabelNode::Vec(c)),
            LabelNodeDe::Array(c) => Ok(LabelNode::Array(c)),
            LabelNodeDe::Struct {
                type_label,
                field_names,
                fields,
            } => Ok(LabelNode::Struct {
                type_label,
                field_names: Cow::Owned(field_names),
                fields: Cow::Owned(fields),
            }),
            LabelNodeDe::Enum {
                type_label,
                variants,
            } => Ok(LabelNode::Enum {
                type_label,
                variants: Cow::Owned(variants),
            }),
            LabelNodeDe::Ref(c) => Ok(LabelNode::Ref(c)),
            LabelNodeDe::Map { key, value } => Ok(LabelNode::Map { key, value }),
        }
    }
}

impl Clone for VariantLabel {
    fn clone(&self) -> Self {
        match self {
            VariantLabel::Unit { name } => VariantLabel::Unit { name: name.clone() },
            VariantLabel::Tuple { name, fields } => VariantLabel::Tuple {
                name: name.clone(),
                fields: fields.clone(),
            },
            VariantLabel::Struct {
                name,
                field_names,
                fields,
            } => VariantLabel::Struct {
                name: name.clone(),
                field_names: field_names.clone(),
                fields: fields.clone(),
            },
        }
    }
}

impl PartialEq for VariantLabel {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (VariantLabel::Unit { name: a }, VariantLabel::Unit { name: b }) => a == b,
            (
                VariantLabel::Tuple {
                    name: na,
                    fields: fa,
                },
                VariantLabel::Tuple {
                    name: nb,
                    fields: fb,
                },
            ) => na == nb && fa == fb,
            (
                VariantLabel::Struct {
                    name: na,
                    field_names: fna,
                    fields: fa,
                },
                VariantLabel::Struct {
                    name: nb,
                    field_names: fnb,
                    fields: fb,
                },
            ) => na == nb && fna == fnb && fa == fb,
            _ => false,
        }
    }
}

impl Eq for VariantLabel {}

impl Serialize for VariantLabel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStructVariant;
        match self {
            VariantLabel::Unit { name } => {
                let mut s = serializer.serialize_struct_variant("VariantLabel", 0, "Unit", 1)?;
                s.serialize_field("name", name)?;
                s.end()
            }
            VariantLabel::Tuple { name, fields } => {
                let mut s = serializer.serialize_struct_variant("VariantLabel", 1, "Tuple", 2)?;
                s.serialize_field("name", name)?;
                s.serialize_field("fields", fields)?;
                s.end()
            }
            VariantLabel::Struct {
                name,
                field_names,
                fields,
            } => {
                let mut s = serializer.serialize_struct_variant("VariantLabel", 2, "Struct", 3)?;
                s.serialize_field("name", name)?;
                s.serialize_field("field_names", field_names)?;
                s.serialize_field("fields", fields)?;
                s.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for VariantLabel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Serialize, Deserialize)]
        enum VariantLabelDe {
            Unit {
                name: Cow<'static, str>,
            },
            Tuple {
                name: Cow<'static, str>,
                fields: Vec<LabelNode>,
            },
            Struct {
                name: Cow<'static, str>,
                field_names: Vec<Cow<'static, str>>,
                fields: Vec<LabelNode>,
            },
        }
        match VariantLabelDe::deserialize(deserializer)? {
            VariantLabelDe::Unit { name } => Ok(VariantLabel::Unit { name }),
            VariantLabelDe::Tuple { name, fields } => Ok(VariantLabel::Tuple {
                name,
                fields: Cow::Owned(fields),
            }),
            VariantLabelDe::Struct {
                name,
                field_names,
                fields,
            } => Ok(VariantLabel::Struct {
                name,
                field_names: Cow::Owned(field_names),
                fields: Cow::Owned(fields),
            }),
        }
    }
}

/// One record in the `aether.kinds.labels` section: the kind's own
/// `Kind::ID` (so the record is self-identifying), the Rust type
/// label, and the parallel-shape `LabelNode` tree. Paired with the
/// matching `SchemaShape` record from `aether.kinds` by id, not by
/// declaration order — any emitter (the Kind derive, `#[handlers]`
/// retention, a third-party shared-rlib wrapper) can write records
/// in any order and the reader will rejoin them correctly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindLabels {
    pub kind_id: u64,
    pub kind_label: Cow<'static, str>,
    pub root: LabelNode,
}

/// One record in the `aether.kinds.inputs` section (ADR-0033). The
/// enum tag discriminates handler vs fallback vs component-level doc
/// so the reader can classify before decoding further. `id` on a
/// `Handler` is the compile-time `K::ID` (ADR-0030); the hub reuses
/// it rather than re-deriving from the name. `doc` values come from
/// rustdoc `///` comments filtered through an optional `# Agent`
/// section — `None` when the source had no comment at all, `Some` of
/// the filtered body otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputsRecord {
    /// A `#[handler]` method's advertised capability.
    Handler {
        id: u64,
        name: Cow<'static, str>,
        doc: Option<Cow<'static, str>>,
    },
    /// A `#[fallback]` method's presence and optional description.
    Fallback { doc: Option<Cow<'static, str>> },
    /// Component-wide rustdoc lifted from the `#[handlers]` impl block.
    Component { doc: Cow<'static, str> },
}

/// Custom-section name for the inputs manifest (ADR-0033). Paired
/// with `aether.kinds` and `aether.kinds.labels`; together they form
/// the component's full declared surface — kinds introduced + kinds
/// handled.
pub const INPUTS_SECTION: &str = "aether.kinds.inputs";

/// Version byte prefixing every record in the `aether.kinds.inputs`
/// section. Follows ADR-0028's per-record versioning convention —
/// unknown versions abort the read rather than silently skip.
pub const INPUTS_SECTION_VERSION: u8 = 0x01;

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
    /// ADR-0042: opaque correlation id the session-originating
    /// caller attached. Echoed verbatim on any reply the engine
    /// emits. `0` means "no correlation" — current MCP `send_mail`
    /// doesn't populate this; tooling that wants end-to-end
    /// correlation sets it explicitly.
    #[serde(default)]
    pub correlation_id: u64,
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
    /// ADR-0042 correlation echo. For session-addressed replies,
    /// the engine copies the `correlation_id` off the inbound
    /// `MailFrame` so the originating session can correlate its
    /// request to the reply. `0` for broadcasts and substrate-
    /// originated mail that didn't originate a correlation.
    #[serde(default)]
    pub correlation_id: u64,
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

/// One captured log entry forwarded from an engine to the hub
/// (ADR-0023). Sequence is monotonic per substrate boot starting at 0
/// — agents poll `engine_logs` with `since: <last>` to consume
/// incrementally without re-receiving entries. `message` is the
/// already-formatted event text (tracing's structured fields are
/// flattened into it); per-line cap is enforced at capture time
/// (>16 KiB truncated with a `...[truncated]` marker).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp_unix_ms: u64,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub sequence: u64,
}

/// Severity for `LogEntry`. Mirrors `tracing::Level`. Ordered
/// most-verbose to least-verbose so a min-level filter can be
/// expressed as `entry.level >= min`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Mail bubbled up from an engine to the hub-substrate (ADR-0037
/// Phase 1). An engine sends this when its local scheduler cannot
/// resolve the target mailbox id — the expected case for a client
/// component addressing a hub-resident component by name
/// (`ctx.resolve_sink::<K>("tic_tac_toe.server")`). Fields are
/// id-only: the sender hashed from the name already, and names
/// don't survive the hash; the hub-substrate looks up the
/// component by id against its own registry.
///
/// `source_mailbox_id` (Phase 2) carries the sending component's
/// local mailbox id so the hub-chassis's reply peripheral can
/// route replies back to it. The source `engine_id` isn't on the
/// wire — the hub knows which TCP connection the frame arrived on.
/// `None` means "no reply target" (broadcast-origin, substrate-
/// generated, no `from_component` attribution); the hub-side
/// sender handle will be `NO_REPLY_HANDLE` for the receiving
/// component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineMailToHubSubstrateFrame {
    pub recipient_mailbox_id: u64,
    pub kind_id: u64,
    pub payload: Vec<u8>,
    pub count: u32,
    pub source_mailbox_id: Option<u64>,
    /// ADR-0042 correlation id the originating component's
    /// `SubstrateCtx::send` minted. Carried across the hub so a
    /// bubbled-up mail's reply (ADR-0037 Phase 2) can echo back
    /// through `MailByIdFrame` and reach a parked `wait_reply_p32`
    /// on the original sender.
    #[serde(default)]
    pub correlation_id: u64,
}

/// Reply mail leaving the hub-substrate for a remote engine's
/// mailbox (ADR-0037 Phase 2). The hub-chassis's reply peripheral
/// emits this when a hub-resident component calls `ctx.reply` on
/// a sender that resolves to `ReplyEntry::RemoteEngineMailbox`.
/// The hub then forwards to the target engine's connection as
/// `HubToEngine::MailById`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailToEngineMailboxFrame {
    pub target_engine_id: EngineId,
    pub target_mailbox_id: u64,
    pub kind_id: u64,
    pub payload: Vec<u8>,
    pub count: u32,
    /// ADR-0042 correlation echo. Set by the reply-emitting engine
    /// so the target engine can correlate the reply to its original
    /// bubble-up request.
    #[serde(default)]
    pub correlation_id: u64,
}

/// Mail delivered to a specific mailbox id on an engine (ADR-0037
/// Phase 2 reply path). Unlike `MailFrame` which carries
/// `recipient_name` (used by `HubToEngine::Mail`), this is strictly
/// id-addressed — replies land without the sender having to know
/// the mailbox's name. The receiver's `HubClient` reader resolves
/// the id against the local `Registry` and pushes onto the
/// `Mailer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailByIdFrame {
    pub recipient_mailbox_id: u64,
    pub kind_id: u64,
    pub payload: Vec<u8>,
    pub count: u32,
    /// ADR-0042 correlation echo. Carries through from
    /// `EngineMailToHubSubstrateFrame.correlation_id` when the hub
    /// forwards a reply for a bubbled-up request back to the
    /// originating engine.
    #[serde(default)]
    pub correlation_id: u64,
}

/// Frames an engine sends to the hub. `Mail` is the observation path
/// (ADR-0008): engine-originated mail addressed to a Claude session
/// or broadcast to all sessions. `KindsChanged` (ADR-0010 §4) tells
/// the hub to replace its cached descriptor list for this engine —
/// needed after `aether.control.load_component` /
/// `aether.control.replace_component` registers a new kind, which the
/// hub would otherwise miss since its cache is pinned at `Hello`.
/// `LogBatch` (ADR-0023) carries captured log entries from the
/// substrate's tracing layer; the hub appends them to a per-engine
/// ring buffer served via the `engine_logs` MCP tool.
/// `MailToHubSubstrate` (ADR-0037 Phase 1) carries mail the engine
/// couldn't deliver locally — the hub-substrate resolves the id
/// against its own registry and dispatches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineToHub {
    Hello(Hello),
    Heartbeat,
    Mail(EngineMailFrame),
    KindsChanged(Vec<KindDescriptor>),
    LogBatch(Vec<LogEntry>),
    Goodbye(Goodbye),
    MailToHubSubstrate(EngineMailToHubSubstrateFrame),
    /// Reply to a remote engine's mailbox (ADR-0037 Phase 2). The
    /// hub looks up the target engine in its registry and forwards
    /// via `HubToEngine::MailById`.
    MailToEngineMailbox(MailToEngineMailboxFrame),
}

/// Frames the hub sends to an engine. `MailById` (ADR-0037
/// Phase 2) is the id-addressed delivery path used for replies
/// routed back to an engine whose component originated a bubbled-
/// up mail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HubToEngine {
    Welcome(Welcome),
    Heartbeat,
    Mail(MailFrame),
    Goodbye(Goodbye),
    MailById(MailByIdFrame),
}
