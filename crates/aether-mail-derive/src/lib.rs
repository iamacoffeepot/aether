//! Proc-macro home for `#[derive(Kind)]` and `#[derive(Schema)]` per
//! ADR-0019 / ADR-0031 / ADR-0032. Kept separate from `aether-mail`
//! because Rust requires proc-macro crates to opt into
//! `proc-macro = true` and forbids them from exporting non-macro
//! items; pairing them in the same crate would force every consumer
//! through the proc-macro toolchain even when they just want the
//! runtime traits.
//!
//! `Kind` emits the `aether_mail::Kind` impl (`const NAME`, `const ID`,
//! optional `const IS_INPUT`) plus the `#[link_section]` statics for
//! both `aether.kinds` (canonical schema bytes) and
//! `aether.kinds.labels` (nominal sidecar). The ID is
//! `fnv1a_64_bytes(canonical_bytes_of_(name, schema))`, matching the
//! substrate-side derivation byte-for-byte (ADR-0030 Phase 2 /
//! ADR-0032). Consumers must also derive (or hand-roll) `Schema`
//! on the type — the Kind derive walks `<Self as Schema>::SCHEMA`
//! for canonical bytes and `<Self as Schema>::LABEL_NODE` for
//! the labels tree.
//!
//! `Schema` emits three consts per impl: `SCHEMA` (the `SchemaType`
//! tree, const-constructible per ADR-0031), `LABEL` (the
//! `Option<&'static str>` Rust type path from `module_path!()`), and
//! `LABEL_NODE` (the parallel-shape labels tree the kind's
//! sidecar record embeds). It also emits `CastEligible` so `repr_c`
//! flags propagate — field types used as cast-shaped payloads get
//! eligibility for free without a second derive.
//!
//! Field-type handling delegates to `<FieldT as Schema>::SCHEMA` /
//! `LABEL_NODE` for all cross-crate resolution. The one exception is
//! `Vec<u8>` — stable Rust forbids the specialization (`Vec<u8>`
//! would overlap `Vec<T>` because `u8: Schema`), so the derive
//! pattern-matches the field type's syntax and emits
//! `SchemaType::Bytes` / `LabelNode::Anonymous` directly when it
//! sees `Vec<u8>`. Every other shape goes through trait dispatch.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, Fields, GenericArgument, Lit, Meta,
    PathArguments, Type, parse_macro_input, spanned::Spanned,
};

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
    let is_input_item = if is_input {
        quote! { const IS_INPUT: bool = true; }
    } else {
        quote! {}
    };

    // ADR-0032 section emission goes through trait dispatch, not a
    // syntactic walker. `<Self as Schema>::SCHEMA` / `::LABEL_NODE`
    // resolve at const-eval after every consumer-side impl is in
    // scope; the canonical serializers below fold to byte arrays at
    // compile time. No quiet skips — a type with no `Schema` impl
    // fails to compile here, which is the behavior ADR-0032 locks
    // in to keep producer/consumer hashes in lockstep.
    let upper = to_screaming_snake_case(&name.to_string());
    let schema_static_ident = quote::format_ident!("__AETHER_SCHEMA_{}", upper);
    let canonical_len_ident = quote::format_ident!("__AETHER_CANONICAL_LEN_{}", upper);
    let canonical_bytes_ident = quote::format_ident!("__AETHER_CANONICAL_BYTES_{}", upper);
    let labels_ident = quote::format_ident!("__AETHER_KIND_LABELS_{}", upper);
    let labels_len_ident = quote::format_ident!("__AETHER_LABELS_LEN_{}", upper);
    let labels_bytes_ident = quote::format_ident!("__AETHER_LABELS_BYTES_{}", upper);
    let kind_static_ident = quote::format_ident!("__AETHER_KIND_MANIFEST_{}", upper);
    let kind_labels_static_ident = quote::format_ident!("__AETHER_KIND_LABELS_MANIFEST_{}", upper);

    // `#[link_section]` is unsafe under edition 2024 — inert data so
    // the practical risk is nil, but the `unsafe(...)` wrapper is
    // required for the attribute to parse. Wasm-target gating keeps
    // the bytes out of native test executables where they'd just
    // bloat the binary with no reader.
    Ok(quote! {
        impl ::aether_mail::Kind for #name {
            const NAME: &'static str = #kind_name;
            const ID: u64 = ::aether_mail::fnv1a_64_bytes(&#canonical_bytes_ident);
            #is_input_item
        }

        // Intermediate `static` holds the schema value — reading
        // `<T as Schema>::SCHEMA` by value in a const expression
        // materializes a temporary whose non-trivial Drop can't run
        // at compile time. Taking `&SCHEMA_STATIC` sidesteps that
        // (statics live for the whole program; destructor never runs).
        static #schema_static_ident: ::aether_mail::__derive_runtime::SchemaType =
            <#name as ::aether_mail::Schema>::SCHEMA;
        const #canonical_len_ident: usize =
            ::aether_mail::__derive_runtime::canonical::canonical_len_kind(
                #kind_name,
                &#schema_static_ident,
            );
        const #canonical_bytes_ident: [u8; #canonical_len_ident] =
            ::aether_mail::__derive_runtime::canonical::canonical_serialize_kind::<#canonical_len_ident>(
                #kind_name,
                &#schema_static_ident,
            );

        // `static`, not `const`, because `KindLabels` holds `Cow`s
        // whose non-trivial Drop impl is barred from const-eval.
        // Statics have program-wide lifetime so the destructor never
        // needs to run at compile time; const-fn serializers reading
        // `&#labels_ident` see a stable `'static` reference.
        static #labels_ident: ::aether_mail::__derive_runtime::KindLabels =
            ::aether_mail::__derive_runtime::KindLabels {
                kind_label: ::aether_mail::__derive_runtime::Cow::Borrowed(
                    ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#name)),
                ),
                root: <#name as ::aether_mail::Schema>::LABEL_NODE,
            };
        const #labels_len_ident: usize =
            ::aether_mail::__derive_runtime::canonical::canonical_len_labels(&#labels_ident);
        const #labels_bytes_ident: [u8; #labels_len_ident] =
            ::aether_mail::__derive_runtime::canonical::canonical_serialize_labels::<#labels_len_ident>(
                &#labels_ident,
            );

        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds")]
        static #kind_static_ident: [u8; #canonical_len_ident + 1] = {
            let mut out = [0u8; #canonical_len_ident + 1];
            out[0] = 0x02;
            let mut i = 0;
            while i < #canonical_len_ident {
                out[i + 1] = #canonical_bytes_ident[i];
                i += 1;
            }
            out
        };

        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds.labels")]
        static #kind_labels_static_ident: [u8; #labels_len_ident + 1] = {
            let mut out = [0u8; #labels_len_ident + 1];
            out[0] = 0x02;
            let mut i = 0;
            while i < #labels_len_ident {
                out[i + 1] = #labels_bytes_ident[i];
                i += 1;
            }
            out
        };
    })
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
    let name_str = name.to_string();
    let (body, label_node_body, cast_eligible_expr) = match &input.data {
        Data::Struct(_) => {
            let fields = struct_fields(input)?;
            let has_repr_c = struct_has_repr_c(&input.attrs);
            (
                expand_schema_struct(&fields)?,
                expand_label_node_struct(&name_str, &fields),
                cast_eligible_expr_for_struct(has_repr_c, &fields),
            )
        }
        Data::Enum(e) => (
            expand_schema_enum(e)?,
            expand_label_node_enum(&name_str, e),
            quote! { false },
        ),
        Data::Union(u) => {
            return Err(syn::Error::new_spanned(
                u.union_token,
                "Schema derive does not support unions",
            ));
        }
    };
    Ok(quote! {
        impl ::aether_mail::Schema for #name {
            const SCHEMA: ::aether_mail::__derive_runtime::SchemaType = #body;
            const LABEL: ::core::option::Option<&'static str> = ::core::option::Option::Some(
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#name)),
            );
            const LABEL_NODE: ::aether_mail::__derive_runtime::LabelNode = #label_node_body;
        }

        impl ::aether_mail::CastEligible for #name {
            const ELIGIBLE: bool = #cast_eligible_expr;
        }
    })
}

/// Emit the `LabelNode::Struct` literal for the type's `LABEL_NODE`
/// const. Field names come from the Rust source; nested-field label
/// nodes resolve via `<FieldT as Schema>::LABEL_NODE` trait dispatch.
/// `Vec<u8>` field specialization: the schema side reports `Bytes`,
/// the labels side reports `Anonymous` (no nominal info for a raw
/// byte buffer).
fn expand_label_node_struct(type_ident: &str, fields: &[FieldInfo]) -> TokenStream2 {
    let field_names = fields.iter().enumerate().map(|(idx, f)| match &f.ident {
        Some(id) => id.to_string(),
        None => idx.to_string(),
    });
    let field_name_entries = field_names.map(|n| {
        quote! { ::aether_mail::__derive_runtime::Cow::Borrowed(#n) }
    });
    let field_node_exprs = fields.iter().map(|f| field_label_node_expr(&f.ty));
    quote! {
        ::aether_mail::__derive_runtime::LabelNode::Struct {
            type_label: ::core::option::Option::Some(
                ::aether_mail::__derive_runtime::Cow::Borrowed(
                    ::core::concat!(::core::module_path!(), "::", #type_ident),
                ),
            ),
            field_names: ::aether_mail::__derive_runtime::Cow::Borrowed(&[
                #( #field_name_entries ),*
            ]),
            fields: ::aether_mail::__derive_runtime::Cow::Borrowed(&[
                #( #field_node_exprs ),*
            ]),
        }
    }
}

fn expand_label_node_enum(type_ident: &str, data: &DataEnum) -> TokenStream2 {
    let variant_entries = data.variants.iter().map(|v| {
        let vname = v.ident.to_string();
        match &v.fields {
            Fields::Unit => quote! {
                ::aether_mail::__derive_runtime::VariantLabel::Unit {
                    name: ::aether_mail::__derive_runtime::Cow::Borrowed(#vname),
                }
            },
            Fields::Unnamed(unnamed) => {
                let field_exprs = unnamed
                    .unnamed
                    .iter()
                    .map(|f| field_label_node_expr(&f.ty));
                quote! {
                    ::aether_mail::__derive_runtime::VariantLabel::Tuple {
                        name: ::aether_mail::__derive_runtime::Cow::Borrowed(#vname),
                        fields: ::aether_mail::__derive_runtime::Cow::Borrowed(&[
                            #( #field_exprs ),*
                        ]),
                    }
                }
            }
            Fields::Named(named) => {
                let field_name_entries = named.named.iter().map(|f| {
                    let fname = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
                    quote! { ::aether_mail::__derive_runtime::Cow::Borrowed(#fname) }
                });
                let field_node_exprs =
                    named.named.iter().map(|f| field_label_node_expr(&f.ty));
                quote! {
                    ::aether_mail::__derive_runtime::VariantLabel::Struct {
                        name: ::aether_mail::__derive_runtime::Cow::Borrowed(#vname),
                        field_names: ::aether_mail::__derive_runtime::Cow::Borrowed(&[
                            #( #field_name_entries ),*
                        ]),
                        fields: ::aether_mail::__derive_runtime::Cow::Borrowed(&[
                            #( #field_node_exprs ),*
                        ]),
                    }
                }
            }
        }
    });
    quote! {
        ::aether_mail::__derive_runtime::LabelNode::Enum {
            type_label: ::core::option::Option::Some(
                ::aether_mail::__derive_runtime::Cow::Borrowed(
                    ::core::concat!(::core::module_path!(), "::", #type_ident),
                ),
            ),
            variants: ::aether_mail::__derive_runtime::Cow::Borrowed(&[
                #( #variant_entries ),*
            ]),
        }
    }
}

/// Expression for a field's `LabelNode` — trait dispatch through
/// `<T as Schema>::LABEL_NODE` for most types. `Vec<u8>` is the one
/// exception (pattern-matched just like `field_type_schema_expr`)
/// and maps to `LabelNode::Anonymous` because `Bytes` carries no
/// structural children to label.
fn field_label_node_expr(ty: &Type) -> TokenStream2 {
    if is_vec_u8(ty) {
        quote! { ::aether_mail::__derive_runtime::LabelNode::Anonymous }
    } else {
        quote! { <#ty as ::aether_mail::Schema>::LABEL_NODE }
    }
}

fn expand_schema_struct(fields: &[FieldInfo]) -> syn::Result<TokenStream2> {
    if fields.is_empty() {
        return Ok(quote! { ::aether_mail::__derive_runtime::SchemaType::Unit });
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
            ::aether_mail::__derive_runtime::NamedField {
                name: ::aether_mail::__derive_runtime::Cow::Borrowed(#name),
                ty: #ty_expr,
            }
        }
    });

    Ok(quote! {
        ::aether_mail::__derive_runtime::SchemaType::Struct {
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
                ::aether_mail::__derive_runtime::EnumVariant::Unit {
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
                    ::aether_mail::__derive_runtime::EnumVariant::Tuple {
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
                        ::aether_mail::__derive_runtime::NamedField {
                            name: ::aether_mail::__derive_runtime::Cow::Borrowed(#fname),
                            ty: #ty_expr,
                        }
                    }
                });
                quote! {
                    ::aether_mail::__derive_runtime::EnumVariant::Struct {
                        name: ::aether_mail::__derive_runtime::Cow::Borrowed(#name),
                        discriminant: #discriminant,
                        fields: ::aether_mail::__derive_runtime::Cow::Borrowed(&[ #( #field_exprs ),* ]),
                    }
                }
            }
        }
    });

    Ok(quote! {
        ::aether_mail::__derive_runtime::SchemaType::Enum {
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
        quote! { ::aether_mail::__derive_runtime::SchemaType::Bytes }
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
