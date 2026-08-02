//! Tests for [`super::super::mailbox::kinds`] — how a `KindId` is
//! derived and registered, and the name / descriptor reads over it.

use std::panic;

use aether_data::canonical::kind_id_from_parts;
use aether_data::{KindDescriptor, SchemaType};

use crate::mail::KindId;
use crate::mail::registry::Registry;
use crate::testing::boot_authority as auth;

#[test]
fn kind_ids_are_derived_from_name_and_schema() {
    let r = Registry::new();
    let a = r.register_kind(&auth(), "aether.tick");
    let b = r.register_kind(&auth(), "aether.key");
    let c = r.register_kind(&auth(), "hello.npc_health");
    // Ids are the fnv1a hash of canonical (name, schema) bytes —
    // distinct names under the same default schema must produce
    // distinct ids, and matching the expected const derivation
    // pins the hash contract with the derive.
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
    assert_eq!(a, KindId(kind_id_from_parts("aether.tick", &SchemaType::Bytes)));
}

#[test]
fn kind_registration_is_idempotent() {
    let r = Registry::new();
    let first = r.register_kind(&auth(), "aether.tick");
    let second = r.register_kind(&auth(), "aether.tick");
    assert_eq!(first, second);
    // Different name produces a different id — the id is a pure
    // function of the input, not an allocation order.
    assert_ne!(r.register_kind(&auth(), "aether.key"), first);
}

#[test]
fn kind_id_lookup() {
    let r = Registry::new();
    let id = r.register_kind(&auth(), "aether.tick");
    assert_eq!(r.kind_id("aether.tick"), Some(id));
    assert!(r.kind_id("absent").is_none());
}

#[test]
fn kind_name_reverse_lookup() {
    let r = Registry::new();
    let a = r.register_kind(&auth(), "aether.tick");
    let b = r.register_kind(&auth(), "aether.key");
    assert_eq!(r.kind_name(a).as_deref(), Some("aether.tick"));
    assert_eq!(r.kind_name(b).as_deref(), Some("aether.key"));
    assert!(r.kind_name(KindId(999)).is_none());
}

fn unit_desc(name: &str) -> KindDescriptor {
    KindDescriptor { name: name.to_string(), schema: SchemaType::Unit }
}

fn cast_struct_desc(name: &str) -> KindDescriptor {
    use aether_data::{NamedField, Primitive};
    KindDescriptor {
        name: name.to_string(),
        schema: SchemaType::Struct {
            repr_c: true,
            fields: vec![NamedField { name: "x".into(), ty: SchemaType::Scalar(Primitive::U32) }].into(),
        },
    }
}

#[test]
fn register_kind_with_descriptor_stores_schema() {
    let r = Registry::new();
    let id = r.register_kind_with_descriptor(&auth(), cast_struct_desc("aether.foo")).expect("fresh name");
    let stored = r.kind_descriptor(id).expect("descriptor present");
    assert_eq!(stored.schema, cast_struct_desc("aether.foo").schema);
}

#[test]
fn register_kind_with_descriptor_is_idempotent_on_match() {
    let r = Registry::new();
    let first = r.register_kind_with_descriptor(&auth(), cast_struct_desc("aether.foo")).expect("first");
    let second =
        r.register_kind_with_descriptor(&auth(), cast_struct_desc("aether.foo")).expect("same schema should succeed");
    assert_eq!(first, second);
}

/// The first registration stores the schema with named fields
/// (e.g. substrate boot via `aether_kinds::descriptors::all()`); a
/// second registration of the same structural kind with stripped
/// names (e.g. reconstructed from a component's `aether.kinds`
/// canonical bytes) must be accepted as idempotent because both
/// produce the same kind id. This is the path `#[actor]`
/// consumer-crate retention relies on for cross-crate kinds that
/// duplicate boot-registered ones.
#[test]
fn register_kind_with_descriptor_accepts_nominal_only_differences() {
    use aether_data::{NamedField, Primitive};
    let r = Registry::new();
    let named_id = r.register_kind_with_descriptor(&auth(), cast_struct_desc("aether.foo")).expect("first");

    let unnamed = KindDescriptor {
        name: "aether.foo".into(),
        schema: SchemaType::Struct {
            repr_c: true,
            fields: vec![NamedField { name: "".into(), ty: SchemaType::Scalar(Primitive::U32) }].into(),
        },
    };
    let unnamed_id =
        r.register_kind_with_descriptor(&auth(), unnamed).expect("same canonical bytes = same id = idempotent");
    assert_eq!(named_id, unnamed_id);

    // Named version stays in the stored slot — first writer wins.
    let stored = r.kind_descriptor(named_id).expect("still there");
    if let SchemaType::Struct { fields, .. } = &stored.schema {
        assert_eq!(fields[0].name, "x");
    } else {
        panic!("expected struct schema");
    }
}

#[test]
fn register_kind_with_descriptor_distinct_schemas_take_distinct_ids() {
    // Pre-ADR-0030-Phase-2 behavior was: same name + different
    // schema = `KindConflict`. Under hashed ids the id IS the
    // `(name, schema)` pair, so two schemas under the same name
    // land in two separate slots — conflict is only reachable via
    // a genuine hash collision. Document the post-Phase-2 shape
    // and let the conflict path stay exercised via the
    // `_is_idempotent_on_match` test (same-id reentry).
    let r = Registry::new();
    let unit_id = r.register_kind_with_descriptor(&auth(), unit_desc("aether.foo")).expect("first");
    let struct_id = r
        .register_kind_with_descriptor(&auth(), cast_struct_desc("aether.foo"))
        .expect("second — different schema, no conflict under hashed ids");
    assert_ne!(unit_id, struct_id);
    assert_eq!(r.kind_descriptor(unit_id).unwrap().schema, SchemaType::Unit);
    assert!(matches!(r.kind_descriptor(struct_id).unwrap().schema, SchemaType::Struct { .. }));
}

#[test]
fn register_kind_defaults_to_bytes() {
    let r = Registry::new();
    let id = r.register_kind(&auth(), "aether.bar");
    let stored = r.kind_descriptor(id).expect("descriptor present");
    assert_eq!(stored.schema, SchemaType::Bytes);
}

#[test]
fn name_only_and_with_descriptor_resolve_to_distinct_ids() {
    // Under hashed ids the id is a function of (name, schema).
    // The same name registered with two different schemas —
    // `Bytes` (via `register_kind`) and a real struct (via
    // `register_kind_with_descriptor`) — produces two *different*
    // ids, each stored under its own slot. `kind_id(name)` returns
    // whichever id was written to `name_index` most recently; this
    // is a test-only hazard and production callers go through
    // `register_kind_with_descriptor` exclusively.
    let r = Registry::new();
    let real = r.register_kind_with_descriptor(&auth(), cast_struct_desc("aether.foo")).expect("real schema");
    let bytes = r.register_kind(&auth(), "aether.foo");
    assert_ne!(real, bytes);
    assert!(matches!(r.kind_descriptor(real).unwrap().schema, SchemaType::Struct { .. }));
    assert!(matches!(r.kind_descriptor(bytes).unwrap().schema, SchemaType::Bytes,));
}
