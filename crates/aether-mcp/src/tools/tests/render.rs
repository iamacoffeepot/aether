#[allow(clippy::wildcard_imports)]
use super::super::*;

/// `render_shape` on a struct kind produces a `{ field: type, … }`
/// one-liner. Using `aether.fs.write` as a representative struct kind —
/// it has named fields with known types.
#[test]
fn render_shape_struct_kind() {
    use aether_kinds::descriptors;
    let write = descriptors::all()
        .into_iter()
        .find(|d| d.name == "aether.fs.write")
        .expect("aether.fs.write in the substrate vocabulary");
    let shape = render_shape(&write.schema);
    assert!(
        shape.starts_with("{ ") && shape.ends_with(" }"),
        "struct shape should be {{ field: type, … }}, got: {shape:?}",
    );
    assert!(
        shape.contains("namespace") && shape.contains("path"),
        "aether.fs.write shape should mention namespace and path, got: {shape:?}",
    );
}

/// `render_shape` on a unit/fieldless kind produces `{}`.
#[test]
fn render_shape_unit_kind() {
    let shape = render_shape(&SchemaType::Unit);
    assert_eq!(shape, "{}", "unit schema should render as {{}}");
}

/// `render_shape` on an enum kind produces `Var1 | Var2(…) | …`
/// with variants separated by ` | `.
#[test]
fn render_shape_enum_kind() {
    use aether_data::{EnumVariant, SchemaType as ST};
    use std::borrow::Cow;
    let schema = ST::Enum {
        variants: Cow::Borrowed(&[
            EnumVariant::Unit {
                name: Cow::Borrowed("Off"),
                discriminant: 0,
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("On"),
                discriminant: 1,
                fields: Cow::Borrowed(&[ST::Bool]),
            },
        ]),
    };
    let shape = render_shape(&schema);
    assert_eq!(
        shape, "Off | On(bool)",
        "enum shape should be Var1 | Var2(inner)"
    );
}
