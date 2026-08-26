//! Schema-derive UI: `skip_serializing_if` is rejected; `default` is accepted.
//!
//! ADR-0118 encodes every declared field. serde drops a skipped field
//! before the positional serializer sees it, so Kind + Schema + Serialize
//! cannot hide the omission. `#[serde(default)]` does not omit bytes.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/accepts_default_wire_field.rs");
    t.pass("tests/ui/accepts_cast_and_nested_enum.rs");
    t.compile_fail("tests/ui/rejects_skipped_struct_field.rs");
    t.compile_fail("tests/ui/rejects_skipped_enum_field.rs");
    t.compile_fail("tests/ui/rejects_out_of_vocabulary_rc.rs");
    t.compile_fail("tests/ui/rejects_out_of_vocabulary_hashset.rs");
}
