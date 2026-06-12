//! ADR-0065 typed id newtypes — `MailboxId`, `KindId`, `HandleId`.
//!
//! Each type is `#[repr(transparent)]` over a `u64` (postcard
//! wire-identical to a raw u64; cast-shape kinds keep their
//! `#[repr(C)]` layout) and exposes a `pub const TYPE_ID: u64`
//! that downstream `Schema` impls (in `aether-data`) emit as
//! `SchemaType::TypeId(Self::TYPE_ID)`. The hub's encoder/decoder
//! dispatch on the `TYPE_ID` value to translate JSON (tagged-string
//! form per ADR-0064) ↔ postcard (u64 varint) at the wire boundary.
//!
//! The underlying `u64` carries the ADR-0064 tag bits (4-bit type
//! discriminator in the high nibble + 60-bit FNV-1a hash in the low
//! 60 bits). `Display` renders the tagged string form, falling back
//! to hex for the reserved-tag sentinels (e.g. `MailboxId::NONE`).

use core::fmt;

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::hash::{
    TYPE_DOMAIN, fnv1a_64_prefixed, mailbox_id_from_name, mailbox_id_from_name_pair,
    thread_id_from_name,
};
use crate::tagged_id::{self, Tag};

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

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            v.try_into().map_err(|_| de::Error::custom("negative id"))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            tagged_id::decode_with_tag(v, self.expected).map_err(de::Error::custom)
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
#[must_use]
pub const fn tag_for_type_id(type_id: u64) -> Option<Tag> {
    if type_id == MailboxId::TYPE_ID {
        Some(Tag::Mailbox)
    } else if type_id == KindId::TYPE_ID {
        Some(Tag::Kind)
    } else if type_id == HandleId::TYPE_ID {
        Some(Tag::Handle)
    } else if type_id == DagId::TYPE_ID {
        Some(Tag::Dag)
    } else if type_id == TransformId::TYPE_ID {
        Some(Tag::Transform)
    } else if type_id == ThreadId::TYPE_ID {
        Some(Tag::Thread)
    } else {
        None
    }
}

/// Resolve a `SchemaType::TypeId(id)` to its canonical type name —
/// the `TYPE_NAME` const on the matching newtype. Surface for
/// `describe_component` and for diagnostic rendering.
#[must_use]
pub const fn type_name_for_type_id(type_id: u64) -> Option<&'static str> {
    if type_id == MailboxId::TYPE_ID {
        Some(MailboxId::TYPE_NAME)
    } else if type_id == KindId::TYPE_ID {
        Some(KindId::TYPE_NAME)
    } else if type_id == HandleId::TYPE_ID {
        Some(HandleId::TYPE_NAME)
    } else if type_id == DagId::TYPE_ID {
        Some(DagId::TYPE_NAME)
    } else if type_id == TransformId::TYPE_ID {
        Some(TransformId::TYPE_NAME)
    } else if type_id == ThreadId::TYPE_ID {
        Some(ThreadId::TYPE_NAME)
    } else {
        None
    }
}

/// Routing token for any mailbox — component or substrate-owned sink.
/// Carries the ADR-0029 deterministic name hash with ADR-0064 tag
/// bits in the high nibble. `#[repr(transparent)]` over `u64` so
/// cast-shape kinds keep their layouts.
#[repr(transparent)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
pub struct MailboxId(pub u64);

impl MailboxId {
    /// Stable type id — FNV-1a of `TYPE_DOMAIN ++ TYPE_NAME`. The
    /// `Schema` impl (in `aether-data`) emits this as
    /// `SchemaType::TypeId(...)`; the hub's codec arms key on it to
    /// pick the JSON/postcard translation.
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.mailbox_id");

    /// Canonical name used to compute `TYPE_ID`. Surfaced by
    /// `describe_component` / tracing as the human-friendly type
    /// label.
    pub const TYPE_NAME: &'static str = "aether.mailbox_id";

    /// Reserved sentinel for "no origin". Registration rejects any
    /// name whose hash collides with 0 (practical probability
    /// ~2⁻⁶⁴, but the guard is cheap) so this id never belongs to a
    /// real mailbox.
    pub const NONE: Self = Self(0);

    /// ADR-0080 §5 chassis-as-mailbox id. Derived from the reserved
    /// name `"aether.chassis"` so it carries normal `Tag::Mailbox` bits
    /// and round-trips through the tagged-string wire form like every
    /// other addressable mailbox (issue iamacoffeepot/aether#725).
    /// Pre-issue-725 this aliased [`Self::NONE`] (= 0); the dual-use of
    /// the zero sentinel for both "uninit/absent" and "chassis sender"
    /// broke the JSON round-trip for chassis-rooted `MailId`s — the
    /// zero id has reserved tag bits that don't encode.
    ///
    /// Mail addressed to `CHASSIS_MAILBOX_ID` is short-circuited by
    /// `Mailer::route_mail` through a chassis-internal switch ahead of
    /// the registry lookup; today that switch handles `Settled { root }`
    /// and signals the gate-site notification map. The chassis is
    /// **not** registered as a real mailbox — `Registry::insert` rejects
    /// any name that hashes to this id so the routing path stays
    /// unambiguous.
    // Canonical chassis mailbox-id constant — this IS a core id definition
    // the resolver builds on, not a sibling-cap address.
    #[allow(clippy::disallowed_methods)]
    pub const CHASSIS_MAILBOX_ID: Self = mailbox_id_from_name("aether.chassis");

    /// Compute the deterministic id for a mailbox name. Same algorithm
    /// the guest SDK uses on the component side — ids round-trip
    /// verbatim across the FFI.
    // Core name→id routing primitive — the runtime-name escape hatch
    // (resolve_actor / wire-Call forwarding) builds on this.
    #[must_use]
    #[allow(clippy::disallowed_methods)]
    pub fn from_name(name: &str) -> Self {
        mailbox_id_from_name(name)
    }
}

impl fmt::Display for MailboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

// Manual `Debug` (not derived) so `?id` in tracing and assert-failure
// messages renders the tagged `mbx-…` form instead of the opaque numeric
// hash — same body as `Display`, since the tag prefix already names the
// id type so a `MailboxId(…)` wrapper would be redundant (iamacoffeepot/aether#1052).
impl fmt::Debug for MailboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
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

/// A node's per-actor identity — *which actor*, independent of where it
/// sits in the tree (ADR-0099 §1). A singleton's `ActorId` is its
/// actor-type tag (ADR-0096), `hash(NAMESPACE)`; an instanced node folds
/// the runtime discriminator in, `hash(NAMESPACE:subname)`. It already
/// carries `Tag::Mailbox` bits, so the depth-1 lineage fold is the
/// identity ([`crate::hash::fold_lineage`]) and a root actor's
/// [`MailboxId`] equals its `ActorId`.
#[repr(transparent)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActorId(pub u64);

impl ActorId {
    /// A singleton node's `ActorId` — the actor-type tag, `hash(NAMESPACE)`.
    // Core actor-type identity hash(NAMESPACE) — the lineage carry folds
    // onto this; it is the id definition, not a sibling-cap address.
    #[must_use]
    #[allow(clippy::disallowed_methods)]
    pub const fn singleton(namespace: &str) -> Self {
        Self(mailbox_id_from_name(namespace).0)
    }

    /// An instanced node's `ActorId` — `hash(NAMESPACE:subname)`, the
    /// namespace with the runtime discriminator folded in by the `:`
    /// cardinality separator (ADR-0079).
    // Core instanced-node identity hash(NAMESPACE:subname) — the id
    // definition, not a sibling-cap address.
    #[must_use]
    #[allow(clippy::disallowed_methods)]
    pub const fn instanced(namespace: &str, subname: &str) -> Self {
        Self(mailbox_id_from_name_pair(namespace, subname).0)
    }
}

/// Schema-hashed identity for a mail kind (ADR-0030). Carries the
/// `Tag::Kind` discriminator + a 60-bit FNV-1a hash of the kind's
/// canonical schema bytes. `#[repr(transparent)]` over `u64`.
#[repr(transparent)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
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

// Tagged `Debug` — see the note on `MailboxId`'s impl.
impl fmt::Debug for KindId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
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
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
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

// Tagged `Debug` — see the note on `MailboxId`'s impl.
impl fmt::Debug for HandleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
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

/// Substrate-minted reference to one submitted computation DAG
/// (ADR-0047 §4). Carries the `Tag::Dag` discriminator + a 60-bit
/// counter masked into the low bits — monotonic-per-substrate with a
/// session salt, analogous to [`HandleId`] rather than a name hash.
/// `#[repr(transparent)]` over `u64`.
#[repr(transparent)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
pub struct DagId(pub u64);

impl DagId {
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.dag_id");
    pub const TYPE_NAME: &'static str = "aether.dag_id";
}

impl fmt::Display for DagId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

// Tagged `Debug` — see the note on `MailboxId`'s impl.
impl fmt::Debug for DagId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

impl Serialize for DagId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(self.0, s)
    }
}

impl<'de> Deserialize<'de> for DagId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(d, Tag::Dag).map(DagId)
    }
}

/// Global identity for a registered native transform (ADR-0048
/// §1/§4). Carries the `Tag::Transform` discriminator + a 60-bit
/// FNV-1a hash of the transform's canonical name. The value
/// derivation (`fnv1a_64(TRANSFORM_DOMAIN ++ canonical("{crate}::\
/// {module}::{fn}"))`) lives with the transform-registry macro work
/// (iamacoffeepot/aether#979); this newtype defines only the wire
/// shape so the descriptor's `Transform` node encodes/decodes.
/// `#[repr(transparent)]` over `u64`.
#[repr(transparent)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
pub struct TransformId(pub u64);

impl TransformId {
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.transform_id");
    pub const TYPE_NAME: &'static str = "aether.transform_id";
}

impl fmt::Display for TransformId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

// Tagged `Debug` — see the note on `MailboxId`'s impl.
impl fmt::Debug for TransformId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

impl Serialize for TransformId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(self.0, s)
    }
}

impl<'de> Deserialize<'de> for TransformId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(d, Tag::Transform).map(TransformId)
    }
}

/// ADR-0088 §7 name-hashed identity for an OS thread (`aether-worker-N`,
/// `aether-root-<NAMESPACE>`, `aether-instanced-<full_name>`). Carries
/// the `Tag::Thread` discriminator + a 60-bit FNV-1a hash of the thread
/// name under `THREAD_DOMAIN`. The dispatch hot path stores this `Copy`
/// id in `TraceEvent::Received` instead of allocating the name string
/// per hop; the cold render path reverses it to a display name through
/// the runtime registry. `#[repr(transparent)]` over `u64`.
#[repr(transparent)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Pod, Zeroable)]
pub struct ThreadId(pub u64);

impl ThreadId {
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.thread_id");
    pub const TYPE_NAME: &'static str = "aether.thread_id";

    /// Compute the deterministic id for an OS thread name. Same
    /// algorithm as [`thread_id_from_name`]; the dispatch
    /// hot path calls this once per worker thread (cached in a
    /// thread-local) so the per-hop cost is a `Copy`, not an alloc.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        thread_id_from_name(name)
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

// Tagged `Debug` — see the note on `MailboxId`'s impl.
impl fmt::Debug for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_tagged(self.0, f)
    }
}

impl Serialize for ThreadId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(self.0, s)
    }
}

impl<'de> Deserialize<'de> for ThreadId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(d, Tag::Thread).map(ThreadId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue iamacoffeepot/aether#725: `CHASSIS_MAILBOX_ID` is a real
    /// `Tag::Mailbox`-tagged id derived from `mailbox_id_from_name(
    /// "aether.chassis")`, distinct from the zero `NONE` sentinel.
    /// Verifies the const isn't accidentally aliased back to NONE and
    /// that it tag-encodes to the standard `mbx-XXXX-XXXX-XXXX` shape
    /// — the serde human-readable branch routes through this same
    /// `tagged_id::encode` path, so round-trip correctness on the JSON
    /// wire follows from this test plus the existing serde tests.
    #[test]
    fn chassis_mailbox_id_is_tagged_and_distinct_from_none() {
        assert_ne!(MailboxId::CHASSIS_MAILBOX_ID, MailboxId::NONE);
        assert_ne!(MailboxId::CHASSIS_MAILBOX_ID.0, 0);
        assert_eq!(
            tagged_id::tag_of(MailboxId::CHASSIS_MAILBOX_ID.0),
            Some(Tag::Mailbox),
        );
        let encoded = tagged_id::encode(MailboxId::CHASSIS_MAILBOX_ID.0)
            .expect("CHASSIS_MAILBOX_ID must tag-encode");
        assert!(encoded.starts_with("mbx-"), "expected tagged: {encoded}");
        let decoded = tagged_id::decode_with_tag(&encoded, Tag::Mailbox)
            .expect("CHASSIS_MAILBOX_ID must round-trip via decode_with_tag");
        assert_eq!(decoded, MailboxId::CHASSIS_MAILBOX_ID.0);
    }

    /// `MailboxId::NONE` keeps its zero-sentinel meaning. Its tag bits
    /// are reserved so `tagged_id::encode` returns `None` — the serde
    /// `serialize_id` helper then falls back to `serialize_u64` and
    /// the wire form is a raw `0`. This is the structural difference
    /// between "no sender / uninit" and "chassis sender".
    #[test]
    fn none_remains_untagged_zero_sentinel() {
        assert_eq!(MailboxId::NONE.0, 0);
        assert_eq!(tagged_id::tag_of(MailboxId::NONE.0), None);
        assert!(tagged_id::encode(MailboxId::NONE.0).is_none());
    }

    /// ADR-0047: a `DagId` carrying `Tag::Dag` bits encodes to the
    /// `dag-XXXX-XXXX-XXXX` form and round-trips through `decode_with_tag`.
    /// `tag_for_type_id(DagId::TYPE_ID)` resolves the codec arm.
    #[test]
    fn dag_id_is_tagged_and_resolves_codec_arm() {
        assert_eq!(tag_for_type_id(DagId::TYPE_ID), Some(Tag::Dag));
        let id = tagged_id::with_tag(Tag::Dag, 0x42);
        let encoded = tagged_id::encode(id).expect("DagId tag-encodes");
        assert!(encoded.starts_with("dag-"), "expected tagged: {encoded}");
        let decoded =
            tagged_id::decode_with_tag(&encoded, Tag::Dag).expect("DagId round-trips via decode");
        assert_eq!(decoded, id);
    }

    /// ADR-0048: a `TransformId` carrying `Tag::Transform` bits encodes
    /// to the `trn-XXXX-XXXX-XXXX` form and resolves its codec arm.
    #[test]
    fn transform_id_is_tagged_and_resolves_codec_arm() {
        assert_eq!(tag_for_type_id(TransformId::TYPE_ID), Some(Tag::Transform));
        let id = tagged_id::with_tag(Tag::Transform, 0x99);
        let encoded = tagged_id::encode(id).expect("TransformId tag-encodes");
        assert!(encoded.starts_with("trn-"), "expected tagged: {encoded}");
        let decoded = tagged_id::decode_with_tag(&encoded, Tag::Transform)
            .expect("TransformId round-trips via decode");
        assert_eq!(decoded, id);
    }

    /// ADR-0088 §7: a `ThreadId` carrying `Tag::Thread` bits encodes to
    /// the `thr-XXXX-XXXX-XXXX` form, resolves its codec arm, and
    /// `from_name` is deterministic + tag-stamped — uniform with the
    /// mailbox / kind id derivation.
    #[test]
    fn thread_id_is_tagged_and_resolves_codec_arm() {
        assert_eq!(tag_for_type_id(ThreadId::TYPE_ID), Some(Tag::Thread));
        assert_eq!(
            type_name_for_type_id(ThreadId::TYPE_ID),
            Some("aether.thread_id")
        );
        let id = tagged_id::with_tag(Tag::Thread, 0x77);
        let encoded = tagged_id::encode(id).expect("ThreadId tag-encodes");
        assert!(encoded.starts_with("thr-"), "expected tagged: {encoded}");
        let decoded = tagged_id::decode_with_tag(&encoded, Tag::Thread)
            .expect("ThreadId round-trips via decode");
        assert_eq!(decoded, id);

        let a = ThreadId::from_name("aether-worker-0");
        let b = ThreadId::from_name("aether-worker-0");
        let c = ThreadId::from_name("aether-worker-1");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(tagged_id::tag_of(a.0), Some(Tag::Thread));
    }
}
