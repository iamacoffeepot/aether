//! Schema vocabulary — `SchemaType` and friends — for describing the
//! structure of typed bytes (ADR-0019). Used by mail dispatch and
//! anyone else encoding/decoding schema-described data (the prompt-
//! system save format being the next consumer).
//!
//! Was previously co-located with the hub channel wire types in
//! `aether-hub-protocol`; ADR-0069 split this universal half out so
//! consumers don't pull a transport crate to describe data shapes.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::{KindId, MailboxId};
use core::ops::Deref;

/// One entry in `Hello.kinds`: a kind-name plus its schema. The hub
/// uses the schema to encode agent-supplied params into the exact
/// bytes the engine expects (cast-shaped or structured, ADR-0019).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindDescriptor {
    pub name: String,
    pub schema: SchemaType,
}

/// One entry in `Hello.mailboxes` (and the `MailboxesChanged` frame).
/// Like [`KindDescriptor`] for kinds: an authoritative snapshot of the
/// substrate's mailbox table, shipped to the hub at handshake and
/// re-shipped after each runtime mailbox add. The hub caches the list
/// and uses `(name, category)` to render type-prefixed labels in
/// trace tool output (issue iamacoffeepot/aether#731).
///
/// `id` is the deterministic [`MailboxId`] hash of `name` (ADR-0029);
/// shipped explicitly so the hub doesn't have to re-hash and so a
/// future categorisation change can't drift the id space.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxDescriptor {
    pub id: MailboxId,
    pub name: String,
    /// Optional coarse classification of the mailbox's role. `None`
    /// means "uncategorised registered mailbox" — the hub falls back
    /// to the raw tagged id with no type prefix.
    pub category: Option<MailboxCategory>,
}

/// Coarse classification of a mailbox's role for downstream
/// presentation (the trace tool's type prefixes per issue 731).
/// Derived from the mailbox name at snapshot time; the substrate
/// stores no per-mailbox category state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxCategory {
    /// A chassis cap or framework-level actor (`aether.input`,
    /// `aether.render`, `aether.audio`, `aether.fs`, `aether.log`,
    /// `aether.component`, `aether.diagnostics`, etc.). Renders as
    /// `actor:NAME`.
    Actor,
    /// A wasm-component trampoline. Full name has the form
    /// `aether.embedded:NAME`. Renders as
    /// `actor:NAME` too — the agent thinks of trampolines as just
    /// another actor; the variant survives so the hub can tell them
    /// apart for filtering / coloring if needed.
    Trampoline,
    /// The chassis-router short-circuit sentinel (`aether.chassis`).
    /// Reachable as a routing target id but never registered with a
    /// real handler — the snapshot includes a synthetic entry so the
    /// hub can resolve trace `sender` fields that name the chassis.
    /// Renders as `chassis:NAME`.
    ChassisSentinel,
}

/// ADR-0019 schema type vocabulary. Describes the structure of a mail
/// kind's payload in enough detail for the hub to encode it from
/// agent-supplied params and the substrate to decode it into a typed
/// value. `Struct.repr_c = true` selects the cast-shaped wire format
/// (raw `#[repr(C)]` bytes); everything else is structured.
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
    /// Issue #232: keyed lookup table. Wire form is the structured
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
    /// hard-codes the per-id encode/decode logic — for v1, the known
    /// type ids are `MailboxId` and `KindId`, both of which are u64
    /// varint on the structured wire and tagged-string on JSON.
    /// Cast-shape size/align is 8 bytes, 8-byte align — same as a
    /// `u64`, so a typed-id field embedded in a `repr_c: true`
    /// struct keeps the parent's cast-eligibility.
    TypeId(u64),
}

/// Recursion-breaking indirection for nested `SchemaType` fields
/// (ADR-0031). `Static(&'static SchemaType)` is the const-literal arm —
/// derives and hand-rolled impls reference the nested type's
/// `<T as Schema>::SCHEMA` through this variant at compile time.
/// `Owned(Box<SchemaType>)` is the wire arm — the hub's wire
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
    #[must_use]
    pub fn owned(schema: SchemaType) -> Self {
        Self::Owned(Box::new(schema))
    }
}

impl Deref for SchemaCell {
    type Target = SchemaType;
    fn deref(&self) -> &SchemaType {
        match self {
            Self::Static(r) => r,
            Self::Owned(b) => b,
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
        Self::Owned(Box::new((**self).clone()))
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
        SchemaType::deserialize(deserializer).map(Self::owned)
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
/// forward-compatible at the wire level.
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
    /// Variant's wire name — matches the `#[serde(rename)]` rename (if
    /// any) or the bare Rust variant identifier. Used on both the
    /// encode and decode sides for lookup and error reporting.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Unit { name, .. } | Self::Tuple { name, .. } | Self::Struct { name, .. } => name,
        }
    }

    /// Wire discriminant — the varint written on the wire before
    /// the variant body. Assigned by the derive at schema-build time
    /// and stable for the life of the kind vocabulary.
    #[must_use]
    pub fn discriminant(&self) -> u32 {
        match self {
            Self::Unit { discriminant, .. }
            | Self::Tuple { discriminant, .. }
            | Self::Struct { discriminant, .. } => *discriminant,
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
/// prefix) to produce `Kind::ID`. Wire-
/// compatible with `SchemaType` at the subset of bytes they share — the
/// canonical serializer emits bytes that deserialize cleanly into
/// `SchemaShape` via `wire::from_bytes`.
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
    Option(Box<Self>),
    Vec(Box<Self>),
    Array {
        element: Box<Self>,
        len: u32,
    },
    Struct {
        fields: Vec<Self>,
        repr_c: bool,
    },
    Enum {
        variants: Vec<VariantShape>,
    },
    Map {
        key: Box<Self>,
        value: Box<Self>,
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
/// the `aether.kinds` section carries (ADR-0032). Wire-compatible
/// with `KindDescriptor` at the `name` field (both serialize as
/// length-prefixed UTF-8), and with the canonical schema bytes at the
/// `schema` field. The hub decodes one `KindShape` per section record
/// via `wire::take_from_bytes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindShape {
    pub name: Cow<'static, str>,
    pub schema: SchemaShape,
}

/// Labels sidecar — parallel-shape tree of nominal information
/// (ADR-0032). Required at the hub; the canonical schema carries no
/// names, so the hub needs the labels section to map MCP JSON params
/// to wire positions and to render `describe_kinds` output.
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
        fields: Cow<'static, [Self]>,
    },
    Enum {
        type_label: Option<Cow<'static, str>>,
        variants: Cow<'static, [VariantLabel]>,
    },
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
    #[must_use]
    pub fn owned(node: LabelNode) -> Self {
        Self::Owned(Box::new(node))
    }
}

impl Deref for LabelCell {
    type Target = LabelNode;
    fn deref(&self) -> &LabelNode {
        match self {
            Self::Static(r) => r,
            Self::Owned(b) => b,
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
        Self::Owned(Box::new((**self).clone()))
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
        LabelNode::deserialize(deserializer).map(Self::owned)
    }
}

impl Clone for LabelNode {
    fn clone(&self) -> Self {
        match self {
            Self::Anonymous => Self::Anonymous,
            Self::Option(c) => Self::Option(c.clone()),
            Self::Vec(c) => Self::Vec(c.clone()),
            Self::Array(c) => Self::Array(c.clone()),
            Self::Struct {
                type_label,
                field_names,
                fields,
            } => Self::Struct {
                type_label: type_label.clone(),
                field_names: field_names.clone(),
                fields: fields.clone(),
            },
            Self::Enum {
                type_label,
                variants,
            } => Self::Enum {
                type_label: type_label.clone(),
                variants: variants.clone(),
            },
            Self::Map { key, value } => Self::Map {
                key: key.clone(),
                value: value.clone(),
            },
        }
    }
}

impl PartialEq for LabelNode {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Anonymous, Self::Anonymous) => true,
            (Self::Option(a), Self::Option(b))
            | (Self::Vec(a), Self::Vec(b))
            | (Self::Array(a), Self::Array(b)) => a == b,
            (
                Self::Struct {
                    type_label: la,
                    field_names: na,
                    fields: fa,
                },
                Self::Struct {
                    type_label: lb,
                    field_names: nb,
                    fields: fb,
                },
            ) => la == lb && na == nb && fa == fb,
            (
                Self::Enum {
                    type_label: la,
                    variants: va,
                },
                Self::Enum {
                    type_label: lb,
                    variants: vb,
                },
            ) => la == lb && va == vb,
            (Self::Map { key: ka, value: va }, Self::Map { key: kb, value: vb }) => {
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
        // over the same variants — the wire format treats each enum arm by
        // position + body.
        use serde::ser::SerializeStructVariant;
        use serde::ser::SerializeTupleVariant;
        match self {
            Self::Anonymous => serializer.serialize_unit_variant("LabelNode", 0, "Anonymous"),
            Self::Option(cell) => {
                let mut s = serializer.serialize_tuple_variant("LabelNode", 1, "Option", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            Self::Vec(cell) => {
                let mut s = serializer.serialize_tuple_variant("LabelNode", 2, "Vec", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            Self::Array(cell) => {
                let mut s = serializer.serialize_tuple_variant("LabelNode", 3, "Array", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            Self::Struct {
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
            Self::Enum {
                type_label,
                variants,
            } => {
                let mut s = serializer.serialize_struct_variant("LabelNode", 5, "Enum", 2)?;
                s.serialize_field("type_label", type_label)?;
                s.serialize_field("variants", variants)?;
                s.end()
            }
            Self::Map { key, value } => {
                let mut s = serializer.serialize_struct_variant("LabelNode", 6, "Map", 2)?;
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
        // the structured enum encoding: varint discriminant, then body.
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
            Map {
                key: LabelCell,
                value: LabelCell,
            },
        }
        match LabelNodeDe::deserialize(deserializer)? {
            LabelNodeDe::Anonymous => Ok(Self::Anonymous),
            LabelNodeDe::Option(c) => Ok(Self::Option(c)),
            LabelNodeDe::Vec(c) => Ok(Self::Vec(c)),
            LabelNodeDe::Array(c) => Ok(Self::Array(c)),
            LabelNodeDe::Struct {
                type_label,
                field_names,
                fields,
            } => Ok(Self::Struct {
                type_label,
                field_names: Cow::Owned(field_names),
                fields: Cow::Owned(fields),
            }),
            LabelNodeDe::Enum {
                type_label,
                variants,
            } => Ok(Self::Enum {
                type_label,
                variants: Cow::Owned(variants),
            }),
            LabelNodeDe::Map { key, value } => Ok(Self::Map { key, value }),
        }
    }
}

impl Clone for VariantLabel {
    fn clone(&self) -> Self {
        match self {
            Self::Unit { name } => Self::Unit { name: name.clone() },
            Self::Tuple { name, fields } => Self::Tuple {
                name: name.clone(),
                fields: fields.clone(),
            },
            Self::Struct {
                name,
                field_names,
                fields,
            } => Self::Struct {
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
            (Self::Unit { name: a }, Self::Unit { name: b }) => a == b,
            (
                Self::Tuple {
                    name: na,
                    fields: fa,
                },
                Self::Tuple {
                    name: nb,
                    fields: fb,
                },
            ) => na == nb && fa == fb,
            (
                Self::Struct {
                    name: na,
                    field_names: fna,
                    fields: fa,
                },
                Self::Struct {
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
            Self::Unit { name } => {
                let mut s = serializer.serialize_struct_variant("VariantLabel", 0, "Unit", 1)?;
                s.serialize_field("name", name)?;
                s.end()
            }
            Self::Tuple { name, fields } => {
                let mut s = serializer.serialize_struct_variant("VariantLabel", 1, "Tuple", 2)?;
                s.serialize_field("name", name)?;
                s.serialize_field("fields", fields)?;
                s.end()
            }
            Self::Struct {
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
            VariantLabelDe::Unit { name } => Ok(Self::Unit { name }),
            VariantLabelDe::Tuple { name, fields } => Ok(Self::Tuple {
                name,
                fields: Cow::Owned(fields),
            }),
            VariantLabelDe::Struct {
                name,
                field_names,
                fields,
            } => Ok(Self::Struct {
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
/// declaration order — any emitter (the Kind derive, `#[actor]`
/// retention, a third-party shared-rlib wrapper) can write records
/// in any order and the reader will rejoin them correctly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindLabels {
    pub kind_id: KindId,
    pub kind_label: Cow<'static, str>,
    pub root: LabelNode,
}

/// A handler's reply class as reported by the manifest (ADR-0112). The
/// successor to ADR-0109's `Option<KindId>` reply field: a single-class
/// handler reports `None` (`-> ()`) or `One(R)` (`-> R` / `-> Pending<R>`);
/// a manual-class handler reports `Manual` (it issues its own replies, so
/// no single static reply kind); a stream-class handler reports
/// `Stream(R)` (reserved — the class isn't built yet). `describe_*`
/// surfaces this so a caller reads the real reply shape, not a `None`
/// that lies for a handler that replies by hand.
///
/// **Variant order is the wire discriminant** — `None` = 0,
/// `One` = 1, `Stream` = 2, `Manual` = 3. The const-fn encoders in
/// [`crate::canonical`] and the macro emission depend on it; do not
/// reorder.
///
/// The `Schema` impl is hand-written (aether-data has no
/// `extern crate self` alias and never self-derives `Schema`, which is
/// behind the optional `derive` feature). The shape mirrors what the
/// derive would emit for this enum so `describe_kinds` renders it the
/// same as any derived enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyContract {
    /// `-> ()` — a single-class handler that replies nothing.
    None,
    /// `-> R` / `-> Pending<R>` — a single-class handler whose reply kind
    /// is `R`.
    One(KindId),
    /// Reserved (ADR-0112): a stream-class handler emitting `R` replies
    /// over time. Not yet reachable — the macro rejects `#[handler::stream]`.
    Stream(KindId),
    /// A manual-class handler that issues its own replies — no single
    /// static reply kind to report.
    Manual,
}

impl crate::Schema for ReplyContract {
    const SCHEMA: SchemaType = SchemaType::Enum {
        variants: Cow::Borrowed(&[
            EnumVariant::Unit {
                name: Cow::Borrowed("None"),
                discriminant: 0,
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("One"),
                discriminant: 1,
                fields: Cow::Borrowed(&[SchemaType::TypeId(KindId::TYPE_ID)]),
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Stream"),
                discriminant: 2,
                fields: Cow::Borrowed(&[SchemaType::TypeId(KindId::TYPE_ID)]),
            },
            EnumVariant::Unit {
                name: Cow::Borrowed("Manual"),
                discriminant: 3,
            },
        ]),
    };

    const LABEL: Option<&'static str> = Some("ReplyContract");

    // Parallel-shape label tree mirroring `SCHEMA`. The `One` / `Stream`
    // fields carry `LabelNode::Anonymous` — identical to
    // `<KindId as Schema>::LABEL_NODE`, since a typed-id field has no
    // nominal sub-shape.
    const LABEL_NODE: LabelNode = LabelNode::Enum {
        type_label: Some(Cow::Borrowed("ReplyContract")),
        variants: Cow::Borrowed(&[
            VariantLabel::Unit {
                name: Cow::Borrowed("None"),
            },
            VariantLabel::Tuple {
                name: Cow::Borrowed("One"),
                fields: Cow::Borrowed(&[LabelNode::Anonymous]),
            },
            VariantLabel::Tuple {
                name: Cow::Borrowed("Stream"),
                fields: Cow::Borrowed(&[LabelNode::Anonymous]),
            },
            VariantLabel::Unit {
                name: Cow::Borrowed("Manual"),
            },
        ]),
    };
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
        id: KindId,
        name: Cow<'static, str>,
        doc: Option<Cow<'static, str>>,
        /// ADR-0112: the handler's reply class — `None` / `One(R)` for a
        /// single-class handler (the ADR-0109 return-type contract),
        /// `Manual` for a manual-class handler that replies by hand,
        /// `Stream(R)` reserved. Lets a caller read the real `In -> Out`
        /// before issuing the call. Successor to ADR-0109's
        /// `Option<KindId>` reply field.
        reply: ReplyContract,
    },
    /// A `#[fallback]` method's presence and optional description.
    Fallback { doc: Option<Cow<'static, str>> },
    /// Component-wide rustdoc lifted from the `#[actor]` impl block.
    Component { doc: Cow<'static, str> },
    /// ADR-0090 (issue 1257): the component's declared boot-config kind.
    /// `id` is the compile-time `<C::Config as Kind>::ID`; `name` is
    /// `C::Config::NAME`. Emitted by `#[actor]` only when the user
    /// declared a `type Config` other than the synthesized `()` — the
    /// reader lifts it into `ComponentCapabilities.config`.
    Config { id: KindId, name: Cow<'static, str> },
    /// ADR-0096: a per-actor boundary marker in a multi-actor module.
    /// `export!(A, B, …)` writes one `ActorBoundary { namespace }` ahead
    /// of each exported type's own handler / fallback / component-doc /
    /// config records, so the reader can group the flat record stream
    /// back into per-type capability sets. `namespace` is the type's
    /// `Addressable::NAMESPACE`; the first boundary names the entry type.
    /// Appended last so its wire variant tag stays additive — a
    /// single-actor module emits no boundary and decodes byte-identically
    /// under the existing reader, so no section-version bump is needed.
    ActorBoundary { namespace: Cow<'static, str> },
}

/// Custom-section name for the inputs manifest (ADR-0033). Paired
/// with `aether.kinds` and `aether.kinds.labels`; together they form
/// the component's full declared surface — kinds introduced + kinds
/// handled.
pub const INPUTS_SECTION: &str = "aether.kinds.inputs";

/// Version byte prefixing every record in the `aether.kinds.inputs`
/// section. Follows ADR-0028's per-record versioning convention —
/// unknown versions abort the read rather than silently skip. v0x02
/// (ADR-0090 / issue 1257) added the `InputsRecord::Config` variant;
/// v0x03 (ADR-0109 / issue 1803) added the `reply` kind id to the
/// `Handler` variant; v0x04 (ADR-0112 / issue 1850) widened that field
/// from `Option<KindId>` to [`ReplyContract`] so a handler's reply
/// *class* (single / manual / stream) is reported, not just a single
/// reply kind; v0x05 (ADR-0118 / issue 1984) moved every record
/// onto the owned aether-wire format (fixed little-endian
/// selectors / ids / counts). A component built before any of these and
/// a substrate after would otherwise disagree on the record shape, so
/// the reader rejects an older version byte loudly — a hard rebuild
/// boundary.
///
/// Distinct from the `aether.kinds` section's own version (also `0x05`,
/// `kind_manifest::KINDS_VERSION`): the two sections version
/// independently and happen to share a number at this revision.
pub const INPUTS_SECTION_VERSION: u8 = 0x05;
