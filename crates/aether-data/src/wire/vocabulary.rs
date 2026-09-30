//! Hand-written [`WireEncode`] / [`WireDecode`] for types that travel the
//! wire but are not `Schema` types — the metaschema vocabulary, labels,
//! descriptors, and canonical records. Specification is the existing
//! serde impls in [`crate::schema`]: `SchemaCell` (and `LabelCell`) encode
//! by dereferencing so Static/Owned are indistinguishable, and decode
//! always produces the owned arm.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use super::Error;
use super::owned::{WireDecode, WireEncode, take_array};
use crate::schema::{
    ActorLineageRecord, EnumVariant, InputsRecord, KindDescriptor, KindLabels, KindShape, LabelCell, LabelNode,
    MAX_SCHEMA_DEPTH, MailboxCategory, MailboxDescriptor, NamedField, Primitive, ReplyContract, SchemaCell,
    SchemaShape, SchemaType, VariantLabel, VariantShape,
};

/// A `u32`-counted sequence whose elements `item` decodes. The recursive
/// schema families read their child lists through this instead of the
/// generic `Vec` / `Cow<[T]>` decode, which could only call `T::decode` and
/// would restart the nesting depth at 0.
fn seq<'de, T>(
    cursor: &mut &'de [u8],
    mut item: impl FnMut(&mut &'de [u8]) -> Result<T, Error>,
) -> Result<Vec<T>, Error> {
    let count = u32::from_le_bytes(take_array(cursor)?) as usize;
    let mut items = Vec::with_capacity(count.min(cursor.len()));
    for _ in 0..count {
        items.push(item(cursor)?);
    }
    Ok(items)
}

/// Refuses a schema node at `depth` past [`MAX_SCHEMA_DEPTH`]. The root is
/// depth 0 and each nested schema adds one, as in `aether-codec`'s walks, so
/// every schema this decode accepts is one those walks follow.
fn check_depth(depth: usize) -> Result<(), Error> {
    if depth > MAX_SCHEMA_DEPTH {
        Err(Error::SchemaTooDeep)
    } else {
        Ok(())
    }
}

fn schema_type(cursor: &mut &[u8], depth: usize) -> Result<SchemaType, Error> {
    check_depth(depth)?;
    let child = |cursor: &mut &[u8]| schema_type(cursor, depth + 1).map(SchemaCell::owned);
    match u32::decode(cursor)? {
        0 => Ok(SchemaType::Unit),
        1 => Ok(SchemaType::Bool),
        2 => Ok(SchemaType::Scalar(Primitive::decode(cursor)?)),
        3 => Ok(SchemaType::String),
        4 => Ok(SchemaType::Bytes),
        5 => Ok(SchemaType::Option(child(cursor)?)),
        6 => Ok(SchemaType::Vec(child(cursor)?)),
        7 => Ok(SchemaType::Array { element: child(cursor)?, len: u32::decode(cursor)? }),
        8 => Ok(SchemaType::Struct {
            fields: Cow::Owned(seq(cursor, |cursor| named_field(cursor, depth + 1))?),
            repr_c: bool::decode(cursor)?,
        }),
        9 => Ok(SchemaType::Enum { variants: Cow::Owned(seq(cursor, |cursor| enum_variant(cursor, depth + 1))?) }),
        10 => Ok(SchemaType::Map { key: child(cursor)?, value: child(cursor)? }),
        11 => Ok(SchemaType::TypeId(u64::decode(cursor)?)),
        12 => Ok(SchemaType::Blob),
        13 => Ok(SchemaType::Ticket { reply: crate::KindId::decode(cursor)? }),
        other => Err(Error::InvalidEnum(other)),
    }
}

/// A struct field whose type sits at `depth`, which its parent has already
/// counted.
fn named_field(cursor: &mut &[u8], depth: usize) -> Result<NamedField, Error> {
    Ok(NamedField { name: Cow::decode(cursor)?, ty: schema_type(cursor, depth)? })
}

/// An enum variant whose field types sit at `depth`, which its parent has
/// already counted.
fn enum_variant(cursor: &mut &[u8], depth: usize) -> Result<EnumVariant, Error> {
    match u32::decode(cursor)? {
        0 => Ok(EnumVariant::Unit { name: Cow::decode(cursor)?, discriminant: u32::decode(cursor)? }),
        1 => Ok(EnumVariant::Tuple {
            name: Cow::decode(cursor)?,
            discriminant: u32::decode(cursor)?,
            fields: Cow::Owned(seq(cursor, |cursor| schema_type(cursor, depth))?),
        }),
        2 => Ok(EnumVariant::Struct {
            name: Cow::decode(cursor)?,
            discriminant: u32::decode(cursor)?,
            fields: Cow::Owned(seq(cursor, |cursor| named_field(cursor, depth))?),
        }),
        other => Err(Error::InvalidEnum(other)),
    }
}

fn schema_shape(cursor: &mut &[u8], depth: usize) -> Result<SchemaShape, Error> {
    check_depth(depth)?;
    let child = |cursor: &mut &[u8]| schema_shape(cursor, depth + 1).map(Box::new);
    match u32::decode(cursor)? {
        0 => Ok(SchemaShape::Unit),
        1 => Ok(SchemaShape::Bool),
        2 => Ok(SchemaShape::Scalar(Primitive::decode(cursor)?)),
        3 => Ok(SchemaShape::String),
        4 => Ok(SchemaShape::Bytes),
        5 => Ok(SchemaShape::Option(child(cursor)?)),
        6 => Ok(SchemaShape::Vec(child(cursor)?)),
        7 => Ok(SchemaShape::Array { element: child(cursor)?, len: u32::decode(cursor)? }),
        8 => Ok(SchemaShape::Struct {
            fields: seq(cursor, |cursor| schema_shape(cursor, depth + 1))?,
            repr_c: bool::decode(cursor)?,
        }),
        9 => Ok(SchemaShape::Enum { variants: seq(cursor, |cursor| variant_shape(cursor, depth + 1))? }),
        10 => Ok(SchemaShape::Map { key: child(cursor)?, value: child(cursor)? }),
        11 => Ok(SchemaShape::TypeId(u64::decode(cursor)?)),
        12 => Ok(SchemaShape::Blob),
        13 => Ok(SchemaShape::Ticket { reply: crate::KindId::decode(cursor)? }),
        other => Err(Error::InvalidEnum(other)),
    }
}

/// A variant shape whose field shapes sit at `depth`, which its parent has
/// already counted.
fn variant_shape(cursor: &mut &[u8], depth: usize) -> Result<VariantShape, Error> {
    match u32::decode(cursor)? {
        0 => Ok(VariantShape::Unit { discriminant: u32::decode(cursor)? }),
        1 => Ok(VariantShape::Tuple {
            discriminant: u32::decode(cursor)?,
            fields: seq(cursor, |cursor| schema_shape(cursor, depth))?,
        }),
        2 => Ok(VariantShape::Struct {
            discriminant: u32::decode(cursor)?,
            fields: seq(cursor, |cursor| schema_shape(cursor, depth))?,
        }),
        other => Err(Error::InvalidEnum(other)),
    }
}

fn label_node(cursor: &mut &[u8], depth: usize) -> Result<LabelNode, Error> {
    check_depth(depth)?;
    let child = |cursor: &mut &[u8]| label_node(cursor, depth + 1).map(LabelCell::owned);
    match u32::decode(cursor)? {
        0 => Ok(LabelNode::Anonymous),
        1 => Ok(LabelNode::Option(child(cursor)?)),
        2 => Ok(LabelNode::Vec(child(cursor)?)),
        3 => Ok(LabelNode::Array(child(cursor)?)),
        4 => Ok(LabelNode::Struct {
            type_label: Option::decode(cursor)?,
            field_names: Cow::decode(cursor)?,
            fields: Cow::Owned(seq(cursor, |cursor| label_node(cursor, depth + 1))?),
        }),
        5 => Ok(LabelNode::Enum {
            type_label: Option::decode(cursor)?,
            variants: Cow::Owned(seq(cursor, |cursor| variant_label(cursor, depth + 1))?),
        }),
        6 => Ok(LabelNode::Map { key: child(cursor)?, value: child(cursor)? }),
        other => Err(Error::InvalidEnum(other)),
    }
}

/// A variant label whose field labels sit at `depth`, which its parent has
/// already counted.
fn variant_label(cursor: &mut &[u8], depth: usize) -> Result<VariantLabel, Error> {
    match u32::decode(cursor)? {
        0 => Ok(VariantLabel::Unit { name: Cow::decode(cursor)? }),
        1 => Ok(VariantLabel::Tuple {
            name: Cow::decode(cursor)?,
            fields: Cow::Owned(seq(cursor, |cursor| label_node(cursor, depth))?),
        }),
        2 => Ok(VariantLabel::Struct {
            name: Cow::decode(cursor)?,
            field_names: Cow::decode(cursor)?,
            fields: Cow::Owned(seq(cursor, |cursor| label_node(cursor, depth))?),
        }),
        other => Err(Error::InvalidEnum(other)),
    }
}

macro_rules! unit_enum {
    ($ty:ty, $($variant:ident = $idx:literal),+ $(,)?) => {
        impl WireEncode for $ty {
            fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
                let selector: u32 = match self {
                    $(Self::$variant => $idx,)+
                };
                selector.encode(out)
            }
        }

        impl<'de> WireDecode<'de> for $ty {
            fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
                match u32::decode(cursor)? {
                    $($idx => Ok(Self::$variant),)+
                    other => Err(Error::InvalidEnum(other)),
                }
            }
        }
    };
}

unit_enum!(Primitive, U8 = 0, U16 = 1, U32 = 2, U64 = 3, I8 = 4, I16 = 5, I32 = 6, I64 = 7, F32 = 8, F64 = 9,);

unit_enum!(MailboxCategory, Actor = 0, Trampoline = 1, ChassisSentinel = 2);

impl WireEncode for SchemaCell {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        (**self).encode(out)
    }
}

impl<'de> WireDecode<'de> for SchemaCell {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        schema_type(cursor, 0).map(Self::owned)
    }
}

impl WireEncode for LabelCell {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        (**self).encode(out)
    }
}

impl<'de> WireDecode<'de> for LabelCell {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        label_node(cursor, 0).map(Self::owned)
    }
}

impl WireEncode for NamedField {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.name.encode(out)?;
        self.ty.encode(out)
    }
}

impl<'de> WireDecode<'de> for NamedField {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        named_field(cursor, 0)
    }
}

impl WireEncode for EnumVariant {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Unit { name, discriminant } => {
                0u32.encode(out)?;
                name.encode(out)?;
                discriminant.encode(out)
            }
            Self::Tuple { name, discriminant, fields } => {
                1u32.encode(out)?;
                name.encode(out)?;
                discriminant.encode(out)?;
                fields.encode(out)
            }
            Self::Struct { name, discriminant, fields } => {
                2u32.encode(out)?;
                name.encode(out)?;
                discriminant.encode(out)?;
                fields.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for EnumVariant {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        enum_variant(cursor, 0)
    }
}

impl WireEncode for SchemaType {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Unit => 0u32.encode(out),
            Self::Bool => 1u32.encode(out),
            Self::Scalar(primitive) => {
                2u32.encode(out)?;
                primitive.encode(out)
            }
            Self::String => 3u32.encode(out),
            Self::Bytes => 4u32.encode(out),
            Self::Option(cell) => {
                5u32.encode(out)?;
                cell.encode(out)
            }
            Self::Vec(cell) => {
                6u32.encode(out)?;
                cell.encode(out)
            }
            Self::Array { element, len } => {
                7u32.encode(out)?;
                element.encode(out)?;
                len.encode(out)
            }
            Self::Struct { fields, repr_c } => {
                8u32.encode(out)?;
                fields.encode(out)?;
                repr_c.encode(out)
            }
            Self::Enum { variants } => {
                9u32.encode(out)?;
                variants.encode(out)
            }
            Self::Map { key, value } => {
                10u32.encode(out)?;
                key.encode(out)?;
                value.encode(out)
            }
            Self::TypeId(id) => {
                11u32.encode(out)?;
                id.encode(out)
            }
            Self::Blob => 12u32.encode(out),
            Self::Ticket { reply } => {
                13u32.encode(out)?;
                reply.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for SchemaType {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        schema_type(cursor, 0)
    }
}

impl WireEncode for SchemaShape {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Unit => 0u32.encode(out),
            Self::Bool => 1u32.encode(out),
            Self::Scalar(primitive) => {
                2u32.encode(out)?;
                primitive.encode(out)
            }
            Self::String => 3u32.encode(out),
            Self::Bytes => 4u32.encode(out),
            Self::Option(inner) => {
                5u32.encode(out)?;
                inner.encode(out)
            }
            Self::Vec(inner) => {
                6u32.encode(out)?;
                inner.encode(out)
            }
            Self::Array { element, len } => {
                7u32.encode(out)?;
                element.encode(out)?;
                len.encode(out)
            }
            Self::Struct { fields, repr_c } => {
                8u32.encode(out)?;
                fields.encode(out)?;
                repr_c.encode(out)
            }
            Self::Enum { variants } => {
                9u32.encode(out)?;
                variants.encode(out)
            }
            Self::Map { key, value } => {
                10u32.encode(out)?;
                key.encode(out)?;
                value.encode(out)
            }
            Self::TypeId(id) => {
                11u32.encode(out)?;
                id.encode(out)
            }
            Self::Blob => 12u32.encode(out),
            Self::Ticket { reply } => {
                13u32.encode(out)?;
                reply.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for SchemaShape {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        schema_shape(cursor, 0)
    }
}

impl WireEncode for VariantShape {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Unit { discriminant } => {
                0u32.encode(out)?;
                discriminant.encode(out)
            }
            Self::Tuple { discriminant, fields } => {
                1u32.encode(out)?;
                discriminant.encode(out)?;
                fields.encode(out)
            }
            Self::Struct { discriminant, fields } => {
                2u32.encode(out)?;
                discriminant.encode(out)?;
                fields.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for VariantShape {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        variant_shape(cursor, 0)
    }
}

impl WireEncode for KindShape {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.name.encode(out)?;
        self.schema.encode(out)
    }
}

impl<'de> WireDecode<'de> for KindShape {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Ok(Self { name: Cow::decode(cursor)?, schema: SchemaShape::decode(cursor)? })
    }
}

impl WireEncode for KindDescriptor {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.name.encode(out)?;
        self.schema.encode(out)
    }
}

impl<'de> WireDecode<'de> for KindDescriptor {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Ok(Self { name: String::decode(cursor)?, schema: SchemaType::decode(cursor)? })
    }
}

impl WireEncode for MailboxDescriptor {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.id.encode(out)?;
        self.name.encode(out)?;
        self.category.encode(out)
    }
}

impl<'de> WireDecode<'de> for MailboxDescriptor {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Ok(Self {
            id: crate::MailboxId::decode(cursor)?,
            name: String::decode(cursor)?,
            category: Option::decode(cursor)?,
        })
    }
}

impl WireEncode for LabelNode {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Anonymous => 0u32.encode(out),
            Self::Option(cell) => {
                1u32.encode(out)?;
                cell.encode(out)
            }
            Self::Vec(cell) => {
                2u32.encode(out)?;
                cell.encode(out)
            }
            Self::Array(cell) => {
                3u32.encode(out)?;
                cell.encode(out)
            }
            Self::Struct { type_label, field_names, fields } => {
                4u32.encode(out)?;
                type_label.encode(out)?;
                field_names.encode(out)?;
                fields.encode(out)
            }
            Self::Enum { type_label, variants } => {
                5u32.encode(out)?;
                type_label.encode(out)?;
                variants.encode(out)
            }
            Self::Map { key, value } => {
                6u32.encode(out)?;
                key.encode(out)?;
                value.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for LabelNode {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        label_node(cursor, 0)
    }
}

impl WireEncode for VariantLabel {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Unit { name } => {
                0u32.encode(out)?;
                name.encode(out)
            }
            Self::Tuple { name, fields } => {
                1u32.encode(out)?;
                name.encode(out)?;
                fields.encode(out)
            }
            Self::Struct { name, field_names, fields } => {
                2u32.encode(out)?;
                name.encode(out)?;
                field_names.encode(out)?;
                fields.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for VariantLabel {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        variant_label(cursor, 0)
    }
}

impl WireEncode for KindLabels {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.kind_id.encode(out)?;
        self.kind_label.encode(out)?;
        self.root.encode(out)
    }
}

impl<'de> WireDecode<'de> for KindLabels {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Ok(Self {
            kind_id: crate::KindId::decode(cursor)?,
            kind_label: Cow::decode(cursor)?,
            root: LabelNode::decode(cursor)?,
        })
    }
}

impl WireEncode for ReplyContract {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::None => 0u32.encode(out),
            Self::One(id) => {
                1u32.encode(out)?;
                id.encode(out)
            }
            Self::Unchecked => 3u32.encode(out),
        }
    }
}

impl<'de> WireDecode<'de> for ReplyContract {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        match u32::decode(cursor)? {
            0 => Ok(Self::None),
            1 => Ok(Self::One(crate::KindId::decode(cursor)?)),
            // Selector 2 is reserved (retired by #6440) and never reused.
            3 => Ok(Self::Unchecked),
            other => Err(Error::InvalidEnum(other)),
        }
    }
}

impl WireEncode for InputsRecord {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Handler { id, name, doc, reply, reason } => {
                0u32.encode(out)?;
                id.encode(out)?;
                name.encode(out)?;
                doc.encode(out)?;
                reply.encode(out)?;
                reason.encode(out)
            }
            Self::Fallback { doc } => {
                1u32.encode(out)?;
                doc.encode(out)
            }
            Self::Component { doc } => {
                2u32.encode(out)?;
                doc.encode(out)
            }
            Self::Config { id, name } => {
                3u32.encode(out)?;
                id.encode(out)?;
                name.encode(out)
            }
            Self::ActorBoundary { namespace } => {
                4u32.encode(out)?;
                namespace.encode(out)
            }
            Self::Dependency { resolver, namespace } => {
                5u32.encode(out)?;
                resolver.encode(out)?;
                namespace.encode(out)
            }
            Self::Instanced => 6u32.encode(out),
        }
    }
}

impl<'de> WireDecode<'de> for InputsRecord {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        match u32::decode(cursor)? {
            0 => Ok(Self::Handler {
                id: crate::KindId::decode(cursor)?,
                name: Cow::decode(cursor)?,
                doc: Option::decode(cursor)?,
                reply: ReplyContract::decode(cursor)?,
                reason: Option::decode(cursor)?,
            }),
            1 => Ok(Self::Fallback { doc: Option::decode(cursor)? }),
            2 => Ok(Self::Component { doc: Cow::decode(cursor)? }),
            3 => Ok(Self::Config { id: crate::KindId::decode(cursor)?, name: Cow::decode(cursor)? }),
            4 => Ok(Self::ActorBoundary { namespace: Cow::decode(cursor)? }),
            5 => Ok(Self::Dependency { resolver: u8::decode(cursor)?, namespace: Cow::decode(cursor)? }),
            6 => Ok(Self::Instanced),
            other => Err(Error::InvalidEnum(other)),
        }
    }
}

impl WireEncode for ActorLineageRecord {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Root { actor, namespace } => {
                0u32.encode(out)?;
                actor.encode(out)?;
                namespace.encode(out)
            }
            Self::Child { parent, child, parent_namespace, child_namespace } => {
                1u32.encode(out)?;
                parent.encode(out)?;
                child.encode(out)?;
                parent_namespace.encode(out)?;
                child_namespace.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for ActorLineageRecord {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        match u32::decode(cursor)? {
            0 => Ok(Self::Root { actor: u64::decode(cursor)?, namespace: Cow::decode(cursor)? }),
            1 => Ok(Self::Child {
                parent: u64::decode(cursor)?,
                child: u64::decode(cursor)?,
                parent_namespace: Cow::decode(cursor)?,
                child_namespace: Cow::decode(cursor)?,
            }),
            other => Err(Error::InvalidEnum(other)),
        }
    }
}
