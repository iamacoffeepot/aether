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

/// Enum shapes spell the externally tagged JSON envelope accepted by
/// `aether-codec`: strings for unit variants and one-key objects for
/// tuple and struct variants.
#[test]
fn render_shape_enum_kind() {
    use aether_data::{EnumVariant, NamedField, Primitive, SchemaType as ST};
    use std::borrow::Cow;
    let schema = ST::Enum {
        variants: Cow::Owned(vec![
            EnumVariant::Unit { name: Cow::Borrowed("Off"), discriminant: 0 },
            EnumVariant::Tuple { name: Cow::Borrowed("On"), discriminant: 1, fields: Cow::Borrowed(&[ST::Bool]) },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Pair"),
                discriminant: 2,
                fields: Cow::Borrowed(&[ST::Bool, ST::Scalar(Primitive::U32)]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Fault"),
                discriminant: 3,
                fields: Cow::Owned(vec![NamedField { name: Cow::Borrowed("reason"), ty: ST::String }]),
            },
        ]),
    };
    let shape = render_shape(&schema);
    assert_eq!(shape, r#""Off" | { "On": bool } | { "Pair": [bool, u32] } | { "Fault": { reason: String } }"#,);
}

/// A nested public enum remains inside its containing request field. The
/// compact shape must therefore lead directly to params the live codec
/// accepts instead of making the variant look like a top-level request key.
#[test]
fn render_shape_nested_window_mode_matches_codec_params() {
    use aether_data::{NamedField, Schema, SchemaCell};
    use aether_kinds::WindowMode;

    let schema = SchemaType::Struct {
        fields: vec![
            NamedField { name: "mode".into(), ty: <WindowMode as Schema>::SCHEMA },
            NamedField { name: "width".into(), ty: SchemaType::Option(SchemaCell::owned(<u32 as Schema>::SCHEMA)) },
            NamedField { name: "height".into(), ty: SchemaType::Option(SchemaCell::owned(<u32 as Schema>::SCHEMA)) },
        ]
        .into(),
        repr_c: false,
    };

    assert_eq!(
        render_shape(&schema),
        concat!(
            r#"{ mode: "Windowed" | "FullscreenBorderless" | "#,
            r#"{ "FullscreenExclusive": { width: u32, height: u32, refresh_mhz: u32 } }, "#,
            "width: Option<u32>, height: Option<u32> }",
        ),
    );
    assert!(
        aether_codec::encode_schema(&serde_json::json!({"mode": "Windowed", "width": 1600, "height": 1200}), &schema,)
            .is_ok(),
        "the documented nested mode params encode",
    );
    assert!(
        aether_codec::encode_schema(&serde_json::json!({"Windowed": {"width": 1600, "height": 1200}}), &schema,)
            .is_err(),
        "the misleading top-level variant form remains outside the descriptor",
    );
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
        assets: Vec::new(),
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
