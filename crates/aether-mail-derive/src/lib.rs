//! Proc-macro home for `#[derive(Kind)]` and `#[derive(Schema)]` per
//! ADR-0019. Kept separate from `aether-mail` because Rust requires
//! proc-macro crates to opt into `proc-macro = true` and forbids them
//! from exporting non-macro items; pairing them in the same crate would
//! force every consumer through the proc-macro toolchain even when they
//! just want the runtime traits.
//!
//! `Kind` emits only the `aether_mail::Kind` impl — a `const NAME` and
//! nothing else. Wasm guests that just want to address a kind by name
//! derive only this and stay free of hub-protocol entirely.
//!
//! `Schema` is opt-in (typically gated on a `descriptors` feature so
//! it expands only in std consumers). It emits *both* the
//! `aether_mail::Schema` impl returning a `SchemaType` AND a
//! `CastEligible` impl whose `ELIGIBLE` const propagates each field's
//! eligibility against `#[repr(C)]` presence. Pairing them here means
//! types used as schema fields (helper structs like `Vertex`) get
//! `CastEligible` for free without needing a separate derive — the
//! Schema derive is the only place that needs eligibility, so it owns
//! the impl.
//!
//! Field-type handling is the trickiest part. For most field types we
//! delegate to `<FieldT as Schema>::schema()` and let the blanket impls
//! in `aether-mail` do the work. The one exception is `Vec<u8>` —
//! stable Rust forbids the specialization (`Vec<u8>` would overlap
//! `Vec<T>` because `u8: Schema`), so the derive pattern-matches the
//! field type's syntax and emits `SchemaType::Bytes` directly when it
//! sees `Vec<u8>`. Every other shape goes through the trait.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, Fields, GenericArgument, Lit, Meta,
    PathArguments, Type, parse_macro_input, spanned::Spanned,
};

mod manifest;

#[proc_macro_derive(Kind, attributes(kind))]
pub fn derive_kind(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_kind(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_derive(Schema, attributes(kind))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_schema(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_kind(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let KindAttr {
        name: kind_name,
        is_input,
    } = parse_kind_attr(&input.attrs)?;
    if let Data::Union(u) = &input.data {
        return Err(syn::Error::new_spanned(
            u.union_token,
            "Kind derive does not support unions",
        ));
    }
    // Only emit `IS_INPUT` when true — relying on the trait default
    // for non-input kinds keeps the generated code minimal and makes
    // grep-for-IS_INPUT-true a reliable audit of the input set.
    let is_input_item = if is_input {
        quote! { const IS_INPUT: bool = true; }
    } else {
        quote! {}
    };
    // ADR-0028: emit the kind's postcard-encoded descriptor into the
    // `aether.kinds` wasm custom section when the type is syntactically
    // resolvable (see `manifest::resolve`). Unresolvable types (nested
    // cross-crate user types) quietly skip emission — the substrate
    // reads what's in the section and falls back to other registration
    // paths for anything it doesn't see there.
    let manifest_static = build_manifest_static(name, &kind_name, &input.data, &input.attrs);
    Ok(quote! {
        impl ::aether_mail::Kind for #name {
            const NAME: &'static str = #kind_name;
            #is_input_item
        }
        #manifest_static
    })
}

fn build_manifest_static(
    type_ident: &syn::Ident,
    kind_name: &str,
    data: &Data,
    attrs: &[Attribute],
) -> TokenStream2 {
    let descriptor = match data {
        Data::Struct(DataStruct { fields, .. }) => {
            let field_infos = collect_fields(fields);
            manifest::struct_descriptor(kind_name, &field_infos, struct_has_repr_c(attrs))
        }
        Data::Enum(e) => manifest::enum_descriptor(kind_name, e),
        Data::Union(_) => return quote! {},
    };
    let Some(descriptor) = descriptor else {
        return quote! {};
    };
    let bytes = manifest::encode_record(&descriptor);
    let len = bytes.len();
    // A per-type static identifier keeps linker errors legible when
    // two derives clash on section boundaries. `#[used]` blocks dead-
    // code elimination from stripping the static before the linker
    // writes it to the section. Wasm-target gating avoids placing
    // these bytes in native test executables where the section is
    // meaningless. Uppercase the type identifier so the generated
    // const satisfies `non_upper_case_globals` — struct names come
    // in as `CamelCase`, statics want `SCREAMING_SNAKE` by convention.
    let upper = to_screaming_snake_case(&type_ident.to_string());
    let static_ident = quote::format_ident!("__AETHER_KIND_MANIFEST_{}", upper);
    // `#[link_section]` is an unsafe attribute under edition 2024
    // — it places the static somewhere the compiler can't reason
    // about. The bytes are inert data, so the practical risk is nil,
    // but the `unsafe(...)` wrapper is still required for the
    // attribute to parse.
    quote! {
        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds")]
        static #static_ident: [u8; #len] = [ #( #bytes ),* ];
    }
}

fn collect_fields(fields: &Fields) -> Vec<FieldInfo> {
    match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| FieldInfo {
                ident: f.ident.clone(),
                ty: f.ty.clone(),
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .map(|f| FieldInfo {
                ident: None,
                ty: f.ty.clone(),
            })
            .collect(),
        Fields::Unit => Vec::new(),
    }
}

fn cast_eligible_expr_for_struct(has_repr_c: bool, fields: &[FieldInfo]) -> TokenStream2 {
    if !has_repr_c {
        return quote! { false };
    }
    if fields.is_empty() {
        return quote! { true };
    }
    let parts = fields.iter().map(|f| {
        let ty = &f.ty;
        quote! { <#ty as ::aether_mail::CastEligible>::ELIGIBLE }
    });
    quote! { #(#parts)&&* }
}

fn expand_schema(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let (body, cast_eligible_expr) = match &input.data {
        Data::Struct(_) => {
            let fields = struct_fields(input)?;
            let has_repr_c = struct_has_repr_c(&input.attrs);
            (
                expand_schema_struct(&fields)?,
                cast_eligible_expr_for_struct(has_repr_c, &fields),
            )
        }
        Data::Enum(e) => (expand_schema_enum(e)?, quote! { false }),
        Data::Union(u) => {
            return Err(syn::Error::new_spanned(
                u.union_token,
                "Schema derive does not support unions",
            ));
        }
    };
    Ok(quote! {
        impl ::aether_mail::Schema for #name {
            const SCHEMA: ::aether_hub_protocol::SchemaType = #body;
        }

        impl ::aether_mail::CastEligible for #name {
            const ELIGIBLE: bool = #cast_eligible_expr;
        }
    })
}

fn expand_schema_struct(fields: &[FieldInfo]) -> syn::Result<TokenStream2> {
    if fields.is_empty() {
        return Ok(quote! { ::aether_hub_protocol::SchemaType::Unit });
    }

    let entries = fields.iter().enumerate().map(|(idx, f)| {
        let name = match &f.ident {
            Some(id) => id.to_string(),
            // Tuple struct field — name positionally so the hub still
            // has something to render in `describe_kinds`. Postcard
            // doesn't care; field names are advisory metadata.
            None => idx.to_string(),
        };
        let ty_expr = field_type_schema_expr(&f.ty);
        quote! {
            ::aether_hub_protocol::NamedField {
                name: ::aether_mail::__derive_runtime::Cow::Borrowed(#name),
                ty: #ty_expr,
            }
        }
    });

    Ok(quote! {
        ::aether_hub_protocol::SchemaType::Struct {
            fields: ::aether_mail::__derive_runtime::Cow::Borrowed(&[ #( #entries ),* ]),
            repr_c: <Self as ::aether_mail::CastEligible>::ELIGIBLE,
        }
    })
}

fn expand_schema_enum(data: &DataEnum) -> syn::Result<TokenStream2> {
    let variant_entries = data.variants.iter().enumerate().map(|(idx, v)| {
        let name = v.ident.to_string();
        let discriminant = idx as u32;
        match &v.fields {
            Fields::Unit => quote! {
                ::aether_hub_protocol::EnumVariant::Unit {
                    name: ::aether_mail::__derive_runtime::Cow::Borrowed(#name),
                    discriminant: #discriminant,
                }
            },
            Fields::Unnamed(unnamed) => {
                let field_exprs = unnamed
                    .unnamed
                    .iter()
                    .map(|f| field_type_schema_expr(&f.ty));
                quote! {
                    ::aether_hub_protocol::EnumVariant::Tuple {
                        name: ::aether_mail::__derive_runtime::Cow::Borrowed(#name),
                        discriminant: #discriminant,
                        fields: ::aether_mail::__derive_runtime::Cow::Borrowed(&[ #( #field_exprs ),* ]),
                    }
                }
            }
            Fields::Named(named) => {
                let field_exprs = named.named.iter().map(|f| {
                    let fname = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
                    let ty_expr = field_type_schema_expr(&f.ty);
                    quote! {
                        ::aether_hub_protocol::NamedField {
                            name: ::aether_mail::__derive_runtime::Cow::Borrowed(#fname),
                            ty: #ty_expr,
                        }
                    }
                });
                quote! {
                    ::aether_hub_protocol::EnumVariant::Struct {
                        name: ::aether_mail::__derive_runtime::Cow::Borrowed(#name),
                        discriminant: #discriminant,
                        fields: ::aether_mail::__derive_runtime::Cow::Borrowed(&[ #( #field_exprs ),* ]),
                    }
                }
            }
        }
    });

    Ok(quote! {
        ::aether_hub_protocol::SchemaType::Enum {
            variants: ::aether_mail::__derive_runtime::Cow::Borrowed(&[ #( #variant_entries ),* ]),
        }
    })
}

// Pattern-match `Vec<u8>` at the field-type level so it lands as
// `SchemaType::Bytes` rather than the generic `Vec(Scalar(U8))`. Every
// other shape delegates to the `Schema` trait's const — wrapped in
// `SchemaCell::Static` at recursive positions so the literal stays
// const-constructible.
fn field_type_schema_expr(ty: &Type) -> TokenStream2 {
    if is_vec_u8(ty) {
        quote! { ::aether_hub_protocol::SchemaType::Bytes }
    } else {
        quote! { <#ty as ::aether_mail::Schema>::SCHEMA }
    }
}

fn is_vec_u8(ty: &Type) -> bool {
    let Type::Path(tp) = ty else { return false };
    let Some(seg) = tp.path.segments.last() else {
        return false;
    };
    if seg.ident != "Vec" {
        return false;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return false;
    };
    let Some(GenericArgument::Type(Type::Path(inner))) = args.args.first() else {
        return false;
    };
    inner.path.is_ident("u8")
}

// ----- attribute and shape helpers --------------------------------------

struct KindAttr {
    name: String,
    is_input: bool,
}

fn parse_kind_attr(attrs: &[Attribute]) -> syn::Result<KindAttr> {
    for attr in attrs {
        if !attr.path().is_ident("kind") {
            continue;
        }
        let mut name: Option<String> = None;
        let mut is_input = false;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let value = meta.value()?;
                let expr: Expr = value.parse()?;
                if let Expr::Lit(lit) = &expr
                    && let Lit::Str(s) = &lit.lit
                {
                    name = Some(s.value());
                    return Ok(());
                }
                return Err(meta.error("`name` must be a string literal"));
            }
            if meta.path.is_ident("input") {
                // Flag-shaped — no `= value`. ADR-0021: marks this
                // kind as a substrate-published input stream so the
                // SDK's auto-subscribe walk catches it at init.
                is_input = true;
                return Ok(());
            }
            Err(meta.error("expected `name = \"...\"` or `input`"))
        })?;
        if let Some(name) = name {
            return Ok(KindAttr { name, is_input });
        }
    }
    Err(syn::Error::new(
        attrs
            .first()
            .map(|a| a.span())
            .unwrap_or_else(proc_macro2::Span::call_site),
        "missing `#[kind(name = \"...\")]` attribute",
    ))
}

fn struct_has_repr_c(attrs: &[Attribute]) -> bool {
    for attr in attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        let Meta::List(list) = &attr.meta else {
            continue;
        };
        let mut has_c = false;
        let _ = list.parse_nested_meta(|meta| {
            if meta.path.is_ident("C") {
                has_c = true;
            }
            Ok(())
        });
        if has_c {
            return true;
        }
    }
    false
}

fn struct_fields(input: &DeriveInput) -> syn::Result<Vec<FieldInfo>> {
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(syn::Error::new_spanned(&input.ident, "expected struct"));
    };
    Ok(match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| FieldInfo {
                ident: f.ident.clone(),
                ty: f.ty.clone(),
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .map(|f| FieldInfo {
                ident: None,
                ty: f.ty.clone(),
            })
            .collect(),
        Fields::Unit => Vec::new(),
    })
}

pub(crate) struct FieldInfo {
    pub(crate) ident: Option<syn::Ident>,
    pub(crate) ty: Type,
}

fn to_screaming_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}
