//! ADR-0028: proc-macro-time construction of a kind's `KindDescriptor`
//! for embedding in the `aether.kinds` wasm custom section.
//!
//! The derive walks field types *syntactically* — it never executes
//! the `Schema` trait, because the consumer hasn't compiled its impls
//! yet when this macro runs. That means only types the macro can
//! recognize from source patterns are resolvable: primitives (`u8`
//! ..`f64`, `bool`), `String`, `Vec<u8>`, `Vec<T>`, `Option<T>`,
//! `[T; N]`, and nested uses of the same set. A field whose type is
//! an unqualified user identifier (e.g. `Vertex`) cannot be resolved
//! here — the macro returns `None` and the caller skips section
//! emission rather than embed a half-manifest. Such kinds fall back
//! to boot-time registration via `aether-kinds::descriptors::all()`
//! or to the component's own `resolve_kind` flow; the section is an
//! opt-in inspection surface, not a hard requirement.
//!
//! The bytes emitted to the section match what the substrate will
//! decode with `postcard::from_bytes::<KindDescriptor>`. Prefixed
//! per-record with a `0x01` version byte so the format can evolve
//! (ADR-0028 §Versioning).

use aether_hub_protocol::{EnumVariant, KindDescriptor, NamedField, Primitive, SchemaType};
use syn::{DataEnum, Fields, GenericArgument, PathArguments, Type};

use crate::FieldInfo;

/// Section-format version byte. Increment when `KindDescriptor`'s
/// postcard shape changes in a way a v1 reader can't lift.
pub const MANIFEST_VERSION: u8 = 0x01;

/// Build a `KindDescriptor` for a struct kind at macro-expansion time.
/// Returns `None` when any field type isn't syntactically resolvable;
/// the derive then skips section emission entirely.
pub fn struct_descriptor(
    name: &str,
    fields: &[FieldInfo],
    has_repr_c: bool,
) -> Option<KindDescriptor> {
    let schema = if fields.is_empty() {
        SchemaType::Unit
    } else {
        let mut named = Vec::with_capacity(fields.len());
        for (idx, f) in fields.iter().enumerate() {
            let fname = match &f.ident {
                Some(id) => id.to_string(),
                None => idx.to_string(),
            };
            named.push(NamedField {
                name: fname,
                ty: resolve(&f.ty)?,
            });
        }
        SchemaType::Struct {
            fields: named,
            repr_c: has_repr_c,
        }
    };
    Some(KindDescriptor {
        name: name.to_string(),
        schema,
    })
}

/// Build a `KindDescriptor` for an enum kind. Variant discriminants
/// match the runtime `Schema` derive: source-order index cast to `u32`.
pub fn enum_descriptor(name: &str, data: &DataEnum) -> Option<KindDescriptor> {
    let mut variants = Vec::with_capacity(data.variants.len());
    for (idx, v) in data.variants.iter().enumerate() {
        let vname = v.ident.to_string();
        let discriminant = idx as u32;
        let variant = match &v.fields {
            Fields::Unit => EnumVariant::Unit {
                name: vname,
                discriminant,
            },
            Fields::Unnamed(u) => {
                let mut tys = Vec::with_capacity(u.unnamed.len());
                for f in &u.unnamed {
                    tys.push(resolve(&f.ty)?);
                }
                EnumVariant::Tuple {
                    name: vname,
                    discriminant,
                    fields: tys,
                }
            }
            Fields::Named(n) => {
                let mut fs = Vec::with_capacity(n.named.len());
                for f in &n.named {
                    let fname = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
                    fs.push(NamedField {
                        name: fname,
                        ty: resolve(&f.ty)?,
                    });
                }
                EnumVariant::Struct {
                    name: vname,
                    discriminant,
                    fields: fs,
                }
            }
        };
        variants.push(variant);
    }
    Some(KindDescriptor {
        name: name.to_string(),
        schema: SchemaType::Enum { variants },
    })
}

/// Serialize a descriptor into the wire bytes the substrate expects:
/// one version byte followed by the postcard-encoded descriptor.
pub fn encode_record(desc: &KindDescriptor) -> Vec<u8> {
    let body = postcard::to_allocvec(desc).expect("postcard encode is infallible for Vec");
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(MANIFEST_VERSION);
    out.extend_from_slice(&body);
    out
}

/// Attempt to resolve a `syn::Type` to a `SchemaType` without invoking
/// any consumer-side trait impls. The recognized vocabulary covers the
/// payload shapes component authors actually reach for; anything else
/// returns `None` and bubbles up to `struct_descriptor` / `enum_descriptor`.
fn resolve(ty: &Type) -> Option<SchemaType> {
    match ty {
        Type::Array(arr) => {
            let element = resolve(&arr.elem)?;
            let len = array_len(&arr.len)?;
            Some(SchemaType::Array {
                element: Box::new(element),
                len,
            })
        }
        Type::Path(tp) => {
            // Bail on qualified self paths (`<T as Trait>::Assoc`) —
            // these are valid Rust but never match the restricted
            // vocabulary the macro handles.
            if tp.qself.is_some() {
                return None;
            }
            let seg = tp.path.segments.last()?;
            let ident = seg.ident.to_string();
            match ident.as_str() {
                "u8" => Some(SchemaType::Scalar(Primitive::U8)),
                "u16" => Some(SchemaType::Scalar(Primitive::U16)),
                "u32" => Some(SchemaType::Scalar(Primitive::U32)),
                "u64" => Some(SchemaType::Scalar(Primitive::U64)),
                "i8" => Some(SchemaType::Scalar(Primitive::I8)),
                "i16" => Some(SchemaType::Scalar(Primitive::I16)),
                "i32" => Some(SchemaType::Scalar(Primitive::I32)),
                "i64" => Some(SchemaType::Scalar(Primitive::I64)),
                "f32" => Some(SchemaType::Scalar(Primitive::F32)),
                "f64" => Some(SchemaType::Scalar(Primitive::F64)),
                "bool" => Some(SchemaType::Bool),
                "String" => Some(SchemaType::String),
                "Vec" => {
                    let inner = first_generic(seg)?;
                    // `Vec<u8>` is canonical `Bytes`; every other `Vec<T>`
                    // recurses. Matches the runtime `Schema` derive's
                    // pattern-match behavior so the schemas agree.
                    if is_u8_ident(inner) {
                        Some(SchemaType::Bytes)
                    } else {
                        Some(SchemaType::Vec(Box::new(resolve(inner)?)))
                    }
                }
                "Option" => {
                    let inner = first_generic(seg)?;
                    Some(SchemaType::Option(Box::new(resolve(inner)?)))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn first_generic(seg: &syn::PathSegment) -> Option<&Type> {
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}

fn is_u8_ident(ty: &Type) -> bool {
    let Type::Path(tp) = ty else { return false };
    tp.path.is_ident("u8")
}

fn array_len(expr: &syn::Expr) -> Option<u32> {
    // Array lengths in kind payloads are always integer literals in
    // practice. Const-generic parameters and arithmetic expressions
    // aren't resolvable at macro-expansion time — bail rather than
    // emit a manifest with a wrong length.
    let syn::Expr::Lit(lit) = expr else {
        return None;
    };
    let syn::Lit::Int(int) = &lit.lit else {
        return None;
    };
    int.base10_parse::<u32>().ok()
}
