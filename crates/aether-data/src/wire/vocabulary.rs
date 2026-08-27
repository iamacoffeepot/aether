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
use super::owned::{WireDecode, WireEncode};
use crate::schema::{
    ActorLineageRecord, EnumVariant, InputsRecord, KindDescriptor, KindLabels, KindShape, LabelCell, LabelNode,
    MailboxCategory, MailboxDescriptor, NamedField, Primitive, ReplyContract, SchemaCell, SchemaShape, SchemaType,
    VariantLabel, VariantShape,
};

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
        SchemaType::decode(cursor).map(Self::owned)
    }
}

impl WireEncode for LabelCell {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        (**self).encode(out)
    }
}

impl<'de> WireDecode<'de> for LabelCell {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        LabelNode::decode(cursor).map(Self::owned)
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
        Ok(Self { name: Cow::decode(cursor)?, ty: SchemaType::decode(cursor)? })
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
        match u32::decode(cursor)? {
            0 => Ok(Self::Unit { name: Cow::decode(cursor)?, discriminant: u32::decode(cursor)? }),
            1 => Ok(Self::Tuple {
                name: Cow::decode(cursor)?,
                discriminant: u32::decode(cursor)?,
                fields: Cow::decode(cursor)?,
            }),
            2 => Ok(Self::Struct {
                name: Cow::decode(cursor)?,
                discriminant: u32::decode(cursor)?,
                fields: Cow::decode(cursor)?,
            }),
            other => Err(Error::InvalidEnum(other)),
        }
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
        }
    }
}

impl<'de> WireDecode<'de> for SchemaType {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        match u32::decode(cursor)? {
            0 => Ok(Self::Unit),
            1 => Ok(Self::Bool),
            2 => Ok(Self::Scalar(Primitive::decode(cursor)?)),
            3 => Ok(Self::String),
            4 => Ok(Self::Bytes),
            5 => Ok(Self::Option(SchemaCell::decode(cursor)?)),
            6 => Ok(Self::Vec(SchemaCell::decode(cursor)?)),
            7 => Ok(Self::Array { element: SchemaCell::decode(cursor)?, len: u32::decode(cursor)? }),
            8 => Ok(Self::Struct { fields: Cow::decode(cursor)?, repr_c: bool::decode(cursor)? }),
            9 => Ok(Self::Enum { variants: Cow::decode(cursor)? }),
            10 => Ok(Self::Map { key: SchemaCell::decode(cursor)?, value: SchemaCell::decode(cursor)? }),
            11 => Ok(Self::TypeId(u64::decode(cursor)?)),
            other => Err(Error::InvalidEnum(other)),
        }
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
        }
    }
}

impl<'de> WireDecode<'de> for SchemaShape {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        match u32::decode(cursor)? {
            0 => Ok(Self::Unit),
            1 => Ok(Self::Bool),
            2 => Ok(Self::Scalar(Primitive::decode(cursor)?)),
            3 => Ok(Self::String),
            4 => Ok(Self::Bytes),
            5 => Ok(Self::Option(Box::decode(cursor)?)),
            6 => Ok(Self::Vec(Box::decode(cursor)?)),
            7 => Ok(Self::Array { element: Box::decode(cursor)?, len: u32::decode(cursor)? }),
            8 => Ok(Self::Struct { fields: Vec::decode(cursor)?, repr_c: bool::decode(cursor)? }),
            9 => Ok(Self::Enum { variants: Vec::decode(cursor)? }),
            10 => Ok(Self::Map { key: Box::decode(cursor)?, value: Box::decode(cursor)? }),
            11 => Ok(Self::TypeId(u64::decode(cursor)?)),
            other => Err(Error::InvalidEnum(other)),
        }
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
        match u32::decode(cursor)? {
            0 => Ok(Self::Unit { discriminant: u32::decode(cursor)? }),
            1 => Ok(Self::Tuple { discriminant: u32::decode(cursor)?, fields: Vec::decode(cursor)? }),
            2 => Ok(Self::Struct { discriminant: u32::decode(cursor)?, fields: Vec::decode(cursor)? }),
            other => Err(Error::InvalidEnum(other)),
        }
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
        match u32::decode(cursor)? {
            0 => Ok(Self::Anonymous),
            1 => Ok(Self::Option(LabelCell::decode(cursor)?)),
            2 => Ok(Self::Vec(LabelCell::decode(cursor)?)),
            3 => Ok(Self::Array(LabelCell::decode(cursor)?)),
            4 => Ok(Self::Struct {
                type_label: Option::decode(cursor)?,
                field_names: Cow::decode(cursor)?,
                fields: Cow::decode(cursor)?,
            }),
            5 => Ok(Self::Enum { type_label: Option::decode(cursor)?, variants: Cow::decode(cursor)? }),
            6 => Ok(Self::Map { key: LabelCell::decode(cursor)?, value: LabelCell::decode(cursor)? }),
            other => Err(Error::InvalidEnum(other)),
        }
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
        match u32::decode(cursor)? {
            0 => Ok(Self::Unit { name: Cow::decode(cursor)? }),
            1 => Ok(Self::Tuple { name: Cow::decode(cursor)?, fields: Cow::decode(cursor)? }),
            2 => Ok(Self::Struct {
                name: Cow::decode(cursor)?,
                field_names: Cow::decode(cursor)?,
                fields: Cow::decode(cursor)?,
            }),
            other => Err(Error::InvalidEnum(other)),
        }
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
            Self::Multi(id) => {
                2u32.encode(out)?;
                id.encode(out)
            }
            Self::Manual => 3u32.encode(out),
        }
    }
}

impl<'de> WireDecode<'de> for ReplyContract {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        match u32::decode(cursor)? {
            0 => Ok(Self::None),
            1 => Ok(Self::One(crate::KindId::decode(cursor)?)),
            2 => Ok(Self::Multi(crate::KindId::decode(cursor)?)),
            3 => Ok(Self::Manual),
            other => Err(Error::InvalidEnum(other)),
        }
    }
}

impl WireEncode for InputsRecord {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Handler { id, name, doc, reply } => {
                0u32.encode(out)?;
                id.encode(out)?;
                name.encode(out)?;
                doc.encode(out)?;
                reply.encode(out)
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
            }),
            1 => Ok(Self::Fallback { doc: Option::decode(cursor)? }),
            2 => Ok(Self::Component { doc: Cow::decode(cursor)? }),
            3 => Ok(Self::Config { id: crate::KindId::decode(cursor)?, name: Cow::decode(cursor)? }),
            4 => Ok(Self::ActorBoundary { namespace: Cow::decode(cursor)? }),
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
            Self::ModuleChild { child, child_namespace } => {
                2u32.encode(out)?;
                child.encode(out)?;
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
            2 => Ok(Self::ModuleChild { child: u64::decode(cursor)?, child_namespace: Cow::decode(cursor)? }),
            other => Err(Error::InvalidEnum(other)),
        }
    }
}
