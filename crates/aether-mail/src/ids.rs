//! ADR-0065 typed id newtypes — `MailboxId`, `KindId`, `HandleId`.
//!
//! Each type is `#[repr(transparent)]` over a `u64` (postcard
//! wire-identical to a raw u64; cast-shape kinds keep their
//! `#[repr(C)]` layout) and exposes a `pub const TYPE_ID: u64`
//! that the `Schema` impl emits as `SchemaType::TypeId(Self::TYPE_ID)`.
//! The hub's encoder/decoder dispatch on the `TYPE_ID` value to
//! translate JSON (tagged-string form per ADR-0064) ↔ postcard
//! (u64 varint) at the wire boundary.
//!
//! The underlying `u64` carries the ADR-0064 tag bits (4-bit type
//! discriminator in the high nibble + 60-bit FNV-1a hash in the low
//! 60 bits). `Display` renders the tagged string form, falling back
//! to hex for the reserved-tag sentinels (e.g. `MailboxId::NONE`).

use core::fmt;

use aether_hub_protocol::{LabelNode, SchemaType};
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::schema::Schema;
use crate::tagged_id::{self, Tag};
use crate::{TYPE_DOMAIN, fnv1a_64_prefixed, mailbox_id_from_name};

/// Shared `Display` body — render tagged-string form when the tag
/// bits are valid, fall back to hex for reserved sentinels.
fn fmt_tagged(id: u64, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match tagged_id::encode(id) {
        Some(s) => f.write_str(&s),
        None => write!(f, "{id:#018x}"),
    }
}

/// Shared serde body for typed-id types. Branches on the
/// serializer's `is_human_readable` flag so binary backends
/// (postcard, the substrate's wire format) get a raw `u64` varint
/// while text backends (JSON, the MCP wire) get the ADR-0064
/// tagged-string form. Falls back to a raw `u64` for reserved-tag
/// sentinels (e.g. `MailboxId::NONE = 0`) so the encoder doesn't
/// error on a sentinel payload.
fn serialize_id<S: Serializer>(id: u64, s: S) -> Result<S::Ok, S::Error> {
    if s.is_human_readable() {
        match tagged_id::encode(id) {
            Some(encoded) => s.serialize_str(&encoded),
            None => s.serialize_u64(id),
        }
    } else {
        s.serialize_u64(id)
    }
}

/// Shared serde body for typed-id types. For human-readable
/// formats (JSON), accepts either a tagged string or a raw u64
/// number (back-compat for callers that haven't migrated). For
/// binary formats (postcard), reads a raw u64 varint — the
/// substrate wire is byte-identical to a `u64` field.
fn deserialize_id<'de, D: Deserializer<'de>>(d: D, expected: Tag) -> Result<u64, D::Error> {
    use serde::de::{self, Visitor};

    struct IdVisitor {
        expected: Tag,
    }

    impl Visitor<'_> for IdVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "tagged id string ({}-XXXX-XXXX-XXXX) or u64 number",
                self.expected.prefix()
            )
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            tagged_id::decode_with_tag(v, self.expected).map_err(de::Error::custom)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            v.try_into().map_err(|_| de::Error::custom("negative id"))
        }
    }

    if d.is_human_readable() {
        d.deserialize_any(IdVisitor { expected })
    } else {
        u64::deserialize(d)
    }
}

/// Resolve a `SchemaType::TypeId(id)` payload to the `Tag` the
/// hub's codec should expect on the JSON side. Returns `None` for
/// any id that doesn't correspond to a known typed-id newtype —
/// callers should treat that as an error (the schema declares a
/// `TypeId` the codec doesn't know how to translate).
///
/// Adding a new typed-id wrapper is one entry here plus its
/// `TYPE_ID`/`TYPE_NAME` consts on the newtype itself.
pub const fn tag_for_type_id(type_id: u64) -> Option<Tag> {
    if type_id == MailboxId::TYPE_ID {
        Some(Tag::Mailbox)
    } else if type_id == KindId::TYPE_ID {
        Some(Tag::Kind)
    } else if type_id == HandleId::TYPE_ID {
        Some(Tag::Handle)
    } else {
        None
    }
}

/// Resolve a `SchemaType::TypeId(id)` to its canonical type name —
/// the `TYPE_NAME` const on the matching newtype. Surface for
/// `describe_component` and for diagnostic rendering.
pub const fn type_name_for_type_id(type_id: u64) -> Option<&'static str> {
    if type_id == MailboxId::TYPE_ID {
        Some(MailboxId::TYPE_NAME)
    } else if type_id == KindId::TYPE_ID {
        Some(KindId::TYPE_NAME)
    } else if type_id == HandleId::TYPE_ID {
        Some(HandleId::TYPE_NAME)
    } else {
        None
    }
}

/// Routing token for any mailbox — component or substrate-owned sink.
/// Carries the ADR-0029 deterministic name hash with ADR-0064 tag
/// bits in the high nibble. `#[repr(transparent)]` over `u64` so
/// cast-shape kinds keep their layouts.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
pub struct MailboxId(pub u64);

impl MailboxId {
    /// Stable type id — FNV-1a of `TYPE_DOMAIN ++ TYPE_NAME`. The
    /// `Schema` impl emits this as `SchemaType::TypeId(...)`; the
    /// hub's codec arms key on it to pick the JSON/postcard
    /// translation.
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.mailbox_id");

    /// Canonical name used to compute `TYPE_ID`. Surfaced by
    /// `describe_component` / tracing as the human-friendly type
    /// label.
    pub const TYPE_NAME: &'static str = "aether.mailbox_id";

    /// Reserved sentinel for "no origin". Registration rejects any
    /// name whose hash collides with 0 (practical probability
    /// ~2⁻⁶⁴, but the guard is cheap) so this id never belongs to a
    /// real mailbox.
    pub const NONE: MailboxId = MailboxId(0);

    /// Compute the deterministic id for a mailbox name. Same algorithm
    /// the guest SDK uses on the component side — ids round-trip
    /// verbatim across the FFI.
    pub fn from_name(name: &str) -> MailboxId {
        MailboxId(mailbox_id_from_name(name))
    }
}

impl fmt::Display for MailboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

impl Schema for MailboxId {
    const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
    const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl Serialize for MailboxId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(self.0, s)
    }
}

impl<'de> Deserialize<'de> for MailboxId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(d, Tag::Mailbox).map(MailboxId)
    }
}

/// Schema-hashed identity for a mail kind (ADR-0030). Carries the
/// `Tag::Kind` discriminator + a 60-bit FNV-1a hash of the kind's
/// canonical schema bytes. `#[repr(transparent)]` over `u64`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
pub struct KindId(pub u64);

impl KindId {
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.kind_id");
    pub const TYPE_NAME: &'static str = "aether.kind_id";
}

impl fmt::Display for KindId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

impl Schema for KindId {
    const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
    const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl Serialize for KindId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(self.0, s)
    }
}

impl<'de> Deserialize<'de> for KindId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(d, Tag::Kind).map(KindId)
    }
}

/// Substrate-minted reference to a parked value in the handle store
/// (ADR-0045). Carries the `Tag::Handle` discriminator + a 60-bit
/// counter masked into the low bits. `#[repr(transparent)]` over
/// `u64`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
pub struct HandleId(pub u64);

impl HandleId {
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.handle_id");
    pub const TYPE_NAME: &'static str = "aether.handle_id";
}

impl fmt::Display for HandleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

impl Schema for HandleId {
    const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
    const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl Serialize for HandleId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(self.0, s)
    }
}

impl<'de> Deserialize<'de> for HandleId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(d, Tag::Handle).map(HandleId)
    }
}
