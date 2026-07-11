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
            EnumVariant::Unit { name: Cow::Borrowed("Off"), discriminant: 0 },
            EnumVariant::Tuple { name: Cow::Borrowed("On"), discriminant: 1, fields: Cow::Borrowed(&[ST::Bool]) },
        ]),
    };
    let shape = render_shape(&schema);
    assert_eq!(shape, "Off | On(bool)", "enum shape should be Var1 | Var2(inner)");
}

/// Tripwire: projection owns the summary-line trim (issue 3006). Multi-line
/// docs collapse to the first non-empty line when `full=false`; `full=true`
/// keeps the wire form byte-identical for every doc field.
#[test]
fn project_capabilities_trims_docs_unless_full() {
    use aether_data::{KindId, ReplyContract};
    use aether_kinds::{ComponentCapabilities, FallbackCapability, HandlerCapability};

    let multi = "First line summary.\n\nBody paragraph that must not appear by default.";
    let leading_blank = "\n\n  Real summary after blanks  \nSecond line.";
    let caps = ComponentCapabilities {
        handlers: vec![HandlerCapability {
            id: KindId(1),
            name: "aether.test.handler".to_owned(),
            doc: Some(multi.to_owned()),
            reply: ReplyContract::None,
        }],
        fallback: Some(FallbackCapability { doc: Some(leading_blank.to_owned()) }),
        doc: Some(multi.to_owned()),
        config: None,
    };

    let summary = project_capabilities(&caps, false);
    assert_eq!(summary.doc.as_deref(), Some("First line summary."));
    assert_eq!(summary.handlers[0].doc.as_deref(), Some("First line summary."));
    assert_eq!(summary.fallback.as_ref().and_then(|f| f.doc.as_deref()), Some("Real summary after blanks"));
    assert_eq!(summary.handlers[0].name, "aether.test.handler");

    let full = project_capabilities(&caps, true);
    assert_eq!(full.doc.as_deref(), Some(multi));
    assert_eq!(full.handlers[0].doc.as_deref(), Some(multi));
    assert_eq!(full.fallback.as_ref().and_then(|f| f.doc.as_deref()), Some(leading_blank));
}
