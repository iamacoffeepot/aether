// Derive codegen builds deeply-nested `quote!` trees from `if let Some(...)` branches;
// `map_or_else` would obscure the control flow. Allow at the crate root because cargo
// doesn't permit `[lints.clippy]` overrides alongside `lints.workspace = true` in the
// manifest (iamacoffeepot/aether#854 Phase 1.a).
#![allow(clippy::option_if_let_else)]

//! Proc-macro home for `#[derive(Kind)]` and `#[derive(Schema)]` per
//! ADR-0019 / ADR-0031 / ADR-0032. Kept separate from `aether-data`
//! because Rust requires proc-macro crates to opt into
//! `proc-macro = true` and forbids them from exporting non-macro
//! items; pairing them in the same crate would force every consumer
//! through the proc-macro toolchain even when they just want the
//! runtime traits.
//!
//! `Kind` emits the `aether_data::Kind` impl (`const NAME`, `const ID`,
//! optional `const IS_INPUT`) plus the `#[link_section]` statics for
//! both `aether.kinds` (canonical schema bytes) and
//! `aether.kinds.labels` (nominal sidecar). The ID is
//! `fnv1a_64_prefixed(KIND_DOMAIN, canonical_bytes_of(name, schema))`,
//! matching the substrate-side derivation byte-for-byte (ADR-0030
//! Phase 2 / ADR-0032). The `KIND_DOMAIN` prefix disjoins the
//! `Kind::ID` space from `MailboxId` (issue #186). Consumers must also derive (or hand-roll) `Schema`
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
use std::mem;
use syn::meta;
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, ExprLit, Fields, FnArg,
    GenericArgument, ImplItem, Item, ItemImpl, ItemMod, Lit, Meta, PathArguments, ReturnType,
    Signature, Type, parse_macro_input,
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

// Single expansion entry point: emits `Kind` impl, optional
// `CastEligible`, manifest consts, and retention statics — the surface
// is wide enough that extracting helpers would force per-helper
// generic-context arguments without saving readability.
#[allow(clippy::too_many_lines)]
fn expand_kind(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let KindAttr { name: kind_name } = parse_kind_attr(&input.attrs)?;
    if let Data::Union(u) = &input.data {
        return Err(syn::Error::new_spanned(
            u.union_token,
            "Kind derive does not support unions",
        ));
    }

    // ADR-0033 wire-shape autodetect: `#[repr(C)]` on the type means
    // the substrate carried it as raw cast bytes (and the user has
    // `#[derive(Pod, Zeroable)]`); anything else is wire-shaped
    // (ADR-0118 `aether_data::wire`, and the user has
    // `#[derive(Serialize, Deserialize)]`). The
    // dispatcher in `#[actor]` calls `Kind::decode_from_bytes` via
    // `Mail::decode_kind::<K>()`; emitting the body per-impl here is
    // what lets that one call site compile against types whose Pod /
    // Deserialize bounds are disjoint.
    let has_repr_c = struct_has_repr_c(&input.attrs);
    let decode_body = if has_repr_c {
        quote! { ::aether_data::__derive_runtime::decode_cast::<Self>(bytes) }
    } else {
        quote! { ::aether_data::__derive_runtime::decode_wire::<Self>(bytes) }
    };
    // Issue #240: encode mirror. Same `#[repr(C)]` autodetect as
    // `decode_body` — a single `Sink::send` call site routes through
    // `Kind::encode_into_bytes`, picking cast or wire at the
    // kind's derive instead of at every send site.
    let encode_body = if has_repr_c {
        quote! { ::aether_data::__derive_runtime::encode_cast::<Self>(self) }
    } else {
        quote! { ::aether_data::__derive_runtime::encode_wire::<Self>(self) }
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
        impl ::aether_data::Kind for #name {
            const NAME: &'static str = #kind_name;
            // ADR-0064: tag the high 4 bits with `Tag::Kind` so kind
            // ids are distinguishable from mailbox / handle ids by
            // bit pattern alone. The `KIND_DOMAIN` byte prefix still
            // rides the FNV input (ADR-0030) — type info ends up
            // encoded in two independent places that cross-check.
            // Issue 466: `Kind::ID` is typed `KindId`; the wrapper
            // wraps the raw `u64` hash. Wire-format sites that need
            // raw bytes call `.0`; dispatch sites compare `KindId` to
            // `KindId` directly.
            const ID: ::aether_data::KindId = ::aether_data::KindId(
                ::aether_data::with_tag(
                    ::aether_data::Tag::Kind,
                    ::aether_data::fnv1a_64_prefixed(
                        ::aether_data::KIND_DOMAIN,
                        &#canonical_bytes_ident,
                    ),
                ),
            );

            fn decode_from_bytes(bytes: &[u8]) -> ::core::option::Option<Self> {
                #decode_body
            }

            fn encode_into_bytes(&self) -> ::aether_data::__derive_runtime::Vec<u8> {
                #encode_body
            }
        }

        // Intermediate `static` holds the schema value — reading
        // `<T as Schema>::SCHEMA` by value in a const expression
        // materializes a temporary whose non-trivial Drop can't run
        // at compile time. Taking `&SCHEMA_STATIC` sidesteps that
        // (statics live for the whole program; destructor never runs).
        static #schema_static_ident: ::aether_data::__derive_runtime::SchemaType =
            <#name as ::aether_data::Schema>::SCHEMA;
        const #canonical_len_ident: usize =
            ::aether_data::__derive_runtime::canonical::canonical_len_kind(
                #kind_name,
                &#schema_static_ident,
            );
        const #canonical_bytes_ident: [u8; #canonical_len_ident] =
            ::aether_data::__derive_runtime::canonical::canonical_serialize_kind::<#canonical_len_ident>(
                #kind_name,
                &#schema_static_ident,
            );

        // `static`, not `const`, because `KindLabels` holds `Cow`s
        // whose non-trivial Drop impl is barred from const-eval.
        // Statics have program-wide lifetime so the destructor never
        // needs to run at compile time; const-fn serializers reading
        // `&#labels_ident` see a stable `'static` reference.
        static #labels_ident: ::aether_data::__derive_runtime::KindLabels =
            ::aether_data::__derive_runtime::KindLabels {
                // Issue 469: `KindLabels.kind_id` is now typed
                // `KindId` (matches `Kind::ID`); pass through directly.
                kind_id: <#name as ::aether_data::Kind>::ID,
                kind_label: ::aether_data::__derive_runtime::Cow::Borrowed(
                    ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#name)),
                ),
                root: <#name as ::aether_data::Schema>::LABEL_NODE,
            };
        const #labels_len_ident: usize =
            ::aether_data::__derive_runtime::canonical::canonical_len_labels(&#labels_ident);
        const #labels_bytes_ident: [u8; #labels_len_ident] =
            ::aether_data::__derive_runtime::canonical::canonical_serialize_labels::<#labels_len_ident>(
                &#labels_ident,
            );

        // ADR-0028 / ADR-0032 / ADR-0118: `aether.kinds` v0x05 ships
        // `[version_byte][canonical_bytes]`, where the canonical bytes are
        // now the owned aether-wire encoding (issue 1984) — so every
        // `Kind::ID` regenerates, gated loudly behind this version byte.
        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds")]
        static #kind_static_ident: [u8; #canonical_len_ident + 1] = {
            let mut out = [0u8; #canonical_len_ident + 1];
            out[0] = 0x05;
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
            // v0x04 (ADR-0118 / issue 1984): the labels record is the owned
            // aether-wire encoding of `KindLabels`. v0x03 made records
            // self-identifying (`kind_id`); the reader still pairs by id.
            out[0] = 0x04;
            let mut i = 0;
            while i < #labels_len_ident {
                out[i + 1] = #labels_bytes_ident[i];
                i += 1;
            }
            out
        };

        // Issue #243: native-side auto-collection. The wasm
        // `aether.kinds` custom-section above carries the canonical
        // bytes for guest-side discovery; on native, the substrate's
        // `descriptors::all()` materializes the Hub-shipped
        // `KindDescriptor` list by iterating these inventory entries.
        // Cfg-gated to non-wasm targets because `inventory` doesn't
        // link on `wasm32-unknown-unknown`.
        #[cfg(not(target_arch = "wasm32"))]
        ::aether_data::__inventory::inventory::submit! {
            ::aether_data::__inventory::DescriptorEntry {
                name: <#name as ::aether_data::Kind>::NAME,
                schema: &#schema_static_ident,
            }
        }
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
        quote! { <#ty as ::aether_data::CastEligible>::ELIGIBLE }
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
        impl ::aether_data::Schema for #name {
            const SCHEMA: ::aether_data::__derive_runtime::SchemaType = #body;
            const LABEL: ::core::option::Option<&'static str> = ::core::option::Option::Some(
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#name)),
            );
            const LABEL_NODE: ::aether_data::__derive_runtime::LabelNode = #label_node_body;
        }

        impl ::aether_data::CastEligible for #name {
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
        quote! { ::aether_data::__derive_runtime::Cow::Borrowed(#n) }
    });
    let field_node_exprs = fields.iter().map(|f| field_label_node_expr(&f.ty));
    quote! {
        ::aether_data::__derive_runtime::LabelNode::Struct {
            type_label: ::core::option::Option::Some(
                ::aether_data::__derive_runtime::Cow::Borrowed(
                    ::core::concat!(::core::module_path!(), "::", #type_ident),
                ),
            ),
            field_names: ::aether_data::__derive_runtime::Cow::Borrowed(&[
                #( #field_name_entries ),*
            ]),
            fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[
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
                ::aether_data::__derive_runtime::VariantLabel::Unit {
                    name: ::aether_data::__derive_runtime::Cow::Borrowed(#vname),
                }
            },
            Fields::Unnamed(unnamed) => {
                let field_exprs = unnamed.unnamed.iter().map(|f| field_label_node_expr(&f.ty));
                quote! {
                    ::aether_data::__derive_runtime::VariantLabel::Tuple {
                        name: ::aether_data::__derive_runtime::Cow::Borrowed(#vname),
                        fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[
                            #( #field_exprs ),*
                        ]),
                    }
                }
            }
            Fields::Named(named) => {
                let field_name_entries = named.named.iter().map(|f| {
                    let fname = f
                        .ident
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    quote! { ::aether_data::__derive_runtime::Cow::Borrowed(#fname) }
                });
                let field_node_exprs = named.named.iter().map(|f| field_label_node_expr(&f.ty));
                quote! {
                    ::aether_data::__derive_runtime::VariantLabel::Struct {
                        name: ::aether_data::__derive_runtime::Cow::Borrowed(#vname),
                        field_names: ::aether_data::__derive_runtime::Cow::Borrowed(&[
                            #( #field_name_entries ),*
                        ]),
                        fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[
                            #( #field_node_exprs ),*
                        ]),
                    }
                }
            }
        }
    });
    quote! {
        ::aether_data::__derive_runtime::LabelNode::Enum {
            type_label: ::core::option::Option::Some(
                ::aether_data::__derive_runtime::Cow::Borrowed(
                    ::core::concat!(::core::module_path!(), "::", #type_ident),
                ),
            ),
            variants: ::aether_data::__derive_runtime::Cow::Borrowed(&[
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
        quote! { ::aether_data::__derive_runtime::LabelNode::Anonymous }
    } else {
        quote! { <#ty as ::aether_data::Schema>::LABEL_NODE }
    }
}

fn expand_schema_struct(fields: &[FieldInfo]) -> syn::Result<TokenStream2> {
    if fields.is_empty() {
        return Ok(quote! { ::aether_data::__derive_runtime::SchemaType::Unit });
    }

    for f in fields {
        reject_hashmap(&f.ty)?;
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
            ::aether_data::__derive_runtime::NamedField {
                name: ::aether_data::__derive_runtime::Cow::Borrowed(#name),
                ty: #ty_expr,
            }
        }
    });

    Ok(quote! {
        ::aether_data::__derive_runtime::SchemaType::Struct {
            fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[ #( #entries ),* ]),
            repr_c: <Self as ::aether_data::CastEligible>::ELIGIBLE,
        }
    })
}

fn expand_schema_enum(data: &DataEnum) -> syn::Result<TokenStream2> {
    for v in &data.variants {
        for f in &v.fields {
            reject_hashmap(&f.ty)?;
        }
    }

    let variant_entries = data.variants.iter().enumerate().map(|(idx, v)| {
        let name = v.ident.to_string();
        // Enum variants past `u32::MAX` aren't a realistic schema; the
        // canonical-bytes wire format stores discriminants as u32.
        #[allow(clippy::cast_possible_truncation)]
        let discriminant = idx as u32;
        match &v.fields {
            Fields::Unit => quote! {
                ::aether_data::__derive_runtime::EnumVariant::Unit {
                    name: ::aether_data::__derive_runtime::Cow::Borrowed(#name),
                    discriminant: #discriminant,
                }
            },
            Fields::Unnamed(unnamed) => {
                let field_exprs = unnamed
                    .unnamed
                    .iter()
                    .map(|f| field_type_schema_expr(&f.ty));
                quote! {
                    ::aether_data::__derive_runtime::EnumVariant::Tuple {
                        name: ::aether_data::__derive_runtime::Cow::Borrowed(#name),
                        discriminant: #discriminant,
                        fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[ #( #field_exprs ),* ]),
                    }
                }
            }
            Fields::Named(named) => {
                let field_exprs = named.named.iter().map(|f| {
                    let fname = f.ident.as_ref().map(ToString::to_string).unwrap_or_default();
                    let ty_expr = field_type_schema_expr(&f.ty);
                    quote! {
                        ::aether_data::__derive_runtime::NamedField {
                            name: ::aether_data::__derive_runtime::Cow::Borrowed(#fname),
                            ty: #ty_expr,
                        }
                    }
                });
                quote! {
                    ::aether_data::__derive_runtime::EnumVariant::Struct {
                        name: ::aether_data::__derive_runtime::Cow::Borrowed(#name),
                        discriminant: #discriminant,
                        fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[ #( #field_exprs ),* ]),
                    }
                }
            }
        }
    });

    Ok(quote! {
        ::aether_data::__derive_runtime::SchemaType::Enum {
            variants: ::aether_data::__derive_runtime::Cow::Borrowed(&[ #( #variant_entries ),* ]),
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
        quote! { ::aether_data::__derive_runtime::SchemaType::Bytes }
    } else {
        quote! { <#ty as ::aether_data::Schema>::SCHEMA }
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

/// Walk a field-type syntactic tree and reject `HashMap` anywhere
/// inside it (issue #232). `HashMap`'s iteration order is hash-state-
/// dependent, which would let two builds of the same kind hash to
/// different `Kind::ID`s — kind ids are derived from canonical schema
/// bytes, so platform-dependent encoding is a wire-correctness bug.
/// `BTreeMap` (sorted by key) is the deterministic alternative; the
/// error message names it explicitly so the fix is one substitution.
///
/// Recurses through `AngleBracketed` generic args so nested forms like
/// `Vec<HashMap<String, String>>` and `Option<HashMap<...>>` are
/// caught too — the nested case would otherwise pass through trait
/// dispatch and emit a less actionable "trait `Schema` not
/// implemented" error pointing at the inner `HashMap`.
fn reject_hashmap(ty: &Type) -> syn::Result<()> {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last()
            && seg.ident == "HashMap"
        {
            return Err(syn::Error::new_spanned(
                ty,
                "HashMap is not allowed in derived kind schemas — its iteration order is \
                 platform-dependent and would diverge canonical schema bytes (and Kind::ID) \
                 across builds. Use `std::collections::BTreeMap` instead, which sorts by key. \
                 See https://github.com/iamacoffeepot/aether/issues/232",
            ));
        }
        for seg in &tp.path.segments {
            if let PathArguments::AngleBracketed(args) = &seg.arguments {
                for arg in &args.args {
                    if let GenericArgument::Type(inner) = arg {
                        reject_hashmap(inner)?;
                    }
                }
            }
        }
    }
    Ok(())
}

struct KindAttr {
    name: String,
}

fn parse_kind_attr(attrs: &[Attribute]) -> syn::Result<KindAttr> {
    for attr in attrs {
        if !attr.path().is_ident("kind") {
            continue;
        }
        let mut name: Option<String> = None;
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
            Err(meta.error("expected `name = \"...\"`"))
        })?;
        if let Some(name) = name {
            return Ok(KindAttr { name });
        }
    }
    Err(syn::Error::new(
        attrs
            .first()
            .map_or_else(proc_macro2::Span::call_site, Spanned::span),
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

// ADR-0033 phase 3: `#[actor]` on an `impl Component for C` block
// is the one receive path for every component. The macro emits:
//
//   (a) An inherent method `__aether_dispatch(&mut self, ctx, mail)
//       -> u32` on `C` that `export!`'s `receive_p32` shim calls. The
//       body matches `mail.kind()` against each `<K as Kind>::ID`
//       const (ADR-0030 Phase 2) and dispatches to the user-written
//       inherent handler method; a `#[fallback]` catches unmatched
//       kinds; strict receivers (no fallback) return
//       `DISPATCH_UNKNOWN_KIND` so the substrate's scheduler logs the
//       miss (issue #142).
//
//   (b) A wrapper around the user's `init` that prepends
//       `ctx.subscribe_input::<K>()` for every `K::IS_INPUT` handler
//       kind. Replaces the ADR-0027 `KindList::resolve_all` walker.
//       Guarded by `if <K as Kind>::IS_INPUT` so non-input kinds
//       compile down to no-ops.
//
//   (c) Two associated consts on `C`'s inherent impl —
//       `__AETHER_INPUTS_MANIFEST_LEN: usize` and
//       `__AETHER_INPUTS_MANIFEST: [u8; …LEN]` — carrying the
//       concatenated `aether.kinds.inputs` record bytes (one record per
//       `#[handler]`, one per `#[fallback]` if present, one for the
//       component-level doc if present, each prefixed with the section
//       version byte). The `#[link_section]` static that pins these
//       bytes into the wasm custom section is emitted by
//       `aether_actor::export!()` in the cdylib root crate, NOT
//       here. Sections only land where `export!()` runs (the cdylib
//       root); transitive rlib pulls of a `#[actor]`-using crate
//       carry only the const data and contribute no section bytes —
//       which is what keeps duplicate Component records from stacking
//       when a cdylib deps on a sibling cdylib's rlib output.
//
// The user's handler methods ride as inherent methods on `C` (since
// `impl Trait for C` can't host non-trait items); helpers go the same
// way. The trait impl retains only `init` and lifecycle hooks.
//
// Rustdoc capture: `///` comments on the impl block (component-level),
// each `#[handler]`, and each `#[fallback]` become MCP-facing prose. If
// a `# Agent` section is present, only that section's body is sent;
// otherwise the full doc is sent. `cargo doc` still renders the whole
// comment — the `# Agent` heading sits alongside `# Safety`/`# Examples`
// as a conventional reader-specific section.

/// Outer attribute on an `impl FfiActor for X` (or `impl Component for X`)
/// block. Reads the `#[handler]` / `#[fallback]` methods inside, then emits:
///
/// - One `impl HandlesKind<K> for X` per handler kind (gates type-driven
///   sender bounds — ADR-0075).
/// - The dispatch table inherent method `__aether_dispatch` that the
///   `export!` shim's `receive_p32` calls.
/// - The `aether.kinds.inputs` manifest consts (substrate reads them via
///   the wasm custom section the cdylib's `export!` pins in).
/// - The `Actor`-trait const re-routing (`NAMESPACE` flows from the impl
///   block into a sibling `impl Actor`).
///
/// Renamed from `#[actor]` in PR A of issue 533. Same behavior; the
/// new name reads as "decorate this actor's impl" — natural now that the
/// macro applies to any actor (and will extend to native chassis caps in
/// a follow-up).
#[proc_macro_attribute]
pub fn actor(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = match parse_actor_opts(attr.into()) {
        Ok(opts) => opts,
        Err(e) => return e.to_compile_error().into(),
    };
    let item = parse_macro_input!(item as ItemImpl);
    match expand_handlers(item, opts) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Parsed `#[actor(...)]` attribute arguments. Only `skip_markers` is
/// recognised today (issue 565: tells the expander not to emit
/// `Actor` + `HandlesKind` impls because a wrapping `#[bridge]`
/// already emitted them as siblings of a cfg-gated module).
#[derive(Default, Clone, Copy)]
struct ActorOpts {
    skip_markers: bool,
}

fn parse_actor_opts(attr: TokenStream2) -> syn::Result<ActorOpts> {
    let mut opts = ActorOpts::default();
    if attr.is_empty() {
        return Ok(opts);
    }
    let parser = meta::parser(|meta| {
        if meta.path.is_ident("skip_markers") {
            opts.skip_markers = true;
            Ok(())
        } else {
            Err(meta.error("unrecognised #[actor] argument; only `skip_markers` is supported"))
        }
    });
    Parser::parse2(parser, attr)?;
    Ok(opts)
}

/// `#[bridge]` — attribute on a `mod foo { ... }` block holding the
/// native-side implementation of an actor (issue 565).
///
/// The mod hosts substrate-side imports, helpers, and a single
/// `#[actor] impl NativeActor for X { ... }` block. Wasm consumers
/// (loading the cap crate via `aether-capabilities` with
/// `default-features = false`) must still see the always-on `Actor`
/// and `HandlesKind<K>` markers so typed sends like
/// `ctx.actor::<X>().send(&kind)` compile-check, but the substrate-side
/// trait impls and helpers can't be in scope on wasm32.
///
/// `#[bridge]`'s expansion splits across that boundary:
///
/// 1. The marker impls (`Actor` + `HandlesKind<K>` per handler kind)
///    are emitted at sibling level to the mod, **outside** any cfg
///    gate — wasm consumers see them.
/// 2. The original mod is emitted with `#[cfg(not(target_arch =
///    "wasm32"))]` injected, with the inner `#[actor]` rewritten to
///    `#[actor(skip_markers)]` so it doesn't duplicate the markers.
///
/// Naming follows the `cxx::bridge` precedent — an attribute on a
/// mod that splits emission across a boundary.
///
/// ## `#[bridge(feature = "name")]`
///
/// Caps whose native impl pulls heavy native-only deps (`render` →
/// wgpu+png, `audio` → cpal) live behind a cargo feature. Without the
/// feature, the inner `#[actor]` block can't compile (its imports are
/// gone). The optional `feature = "name"` argument adds the feature
/// to the cfg keying both the wasm stub vs. native re-export and the
/// inner `mod native` itself: stub when wasm32 OR the feature is off;
/// re-export when native AND the feature is on. This keeps the
/// always-on markers reachable for wasm components (so they can write
/// `ctx.actor::<RenderCapability>().send(&triangle)`) without the
/// feature pulling its native dep set in.
#[proc_macro_attribute]
pub fn bridge(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = match parse_bridge_attr(attr) {
        Ok(o) => o,
        Err(e) => return e.to_compile_error().into(),
    };
    let item = parse_macro_input!(item as ItemMod);
    match expand_bridge(item, opts) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Cardinality declaration on `#[bridge]`. The bridge attribute is the
/// natural site for this because the bridge owns two struct definition
/// sites (the wasm stub at file root + the native struct in `mod
/// native`), and a cardinality marker impl needs to land at file root
/// to cover both. Pre-issue-625 the bridge auto-emitted Singleton; the
/// refactor makes the choice explicit (and adds Instanced as the
/// counterpart).
#[derive(Clone, Copy)]
enum BridgeCardinality {
    Singleton,
    Instanced,
}

#[derive(Default)]
struct BridgeOpts {
    cardinality: Option<BridgeCardinality>,
    feature: Option<String>,
    /// The `one_per = "entity"` instance-cardinality declaration
    /// (ADR-0088 §4 v2). Only meaningful with `instanced`: it rides into
    /// the emitted `TemplateEntry` as `Cardinality::OnePer(entity)`,
    /// making the reverse-lookup manifest self-describing ("one mailbox
    /// per loaded component") instead of an opaque `Dynamic` family.
    /// Absent on an instanced actor ⇒ `Cardinality::Unbounded`.
    one_per: Option<String>,
}

/// Parse `#[bridge]`'s optional arguments. Recognised:
/// - `singleton` (positional flag) — emit `impl Singleton for X`
/// - `instanced` (positional flag) — emit `impl Instanced for X`
/// - `one_per = "entity"` — instanced cardinality (ADR-0088 §4 v2);
///   `instanced`-only
/// - `feature = "name"` — gate the inner mod on the named feature
///
/// Empty attr (`#[bridge]`) emits no cardinality marker; the author is
/// expected to hand-roll one (test fixtures, future cases). Mixing
/// `singleton` and `instanced` is rejected — a cap is one or the other.
/// `one_per` on a non-instanced bridge is rejected (the entity-relationship
/// only describes an instanced family); it is validated in `expand_bridge`
/// where both flags are known, since attribute arg order is unspecified.
fn parse_bridge_attr(attr: TokenStream) -> syn::Result<BridgeOpts> {
    let mut opts = BridgeOpts::default();
    if attr.is_empty() {
        return Ok(opts);
    }
    let parser = meta::parser(|meta| {
        if meta.path.is_ident("singleton") {
            if matches!(opts.cardinality, Some(BridgeCardinality::Instanced)) {
                return Err(meta.error(
                    "#[bridge] cannot declare both `singleton` and `instanced` — \
                     cardinality is mutually exclusive (ADR-0079)",
                ));
            }
            opts.cardinality = Some(BridgeCardinality::Singleton);
            Ok(())
        } else if meta.path.is_ident("instanced") {
            if matches!(opts.cardinality, Some(BridgeCardinality::Singleton)) {
                return Err(meta.error(
                    "#[bridge] cannot declare both `singleton` and `instanced` — \
                     cardinality is mutually exclusive (ADR-0079)",
                ));
            }
            opts.cardinality = Some(BridgeCardinality::Instanced);
            Ok(())
        } else if meta.path.is_ident("one_per") {
            let value = meta.value()?;
            let lit: syn::LitStr = value.parse()?;
            opts.one_per = Some(lit.value());
            Ok(())
        } else if meta.path.is_ident("feature") {
            let value = meta.value()?;
            let lit: syn::LitStr = value.parse()?;
            opts.feature = Some(lit.value());
            Ok(())
        } else {
            Err(meta.error(
                "#[bridge] only accepts `singleton`, `instanced`, \
                 `one_per = \"entity\"`, or `feature = \"name\"`",
            ))
        }
    });
    Parser::parse(parser, attr)?;
    Ok(opts)
}

// Single-pass `#[bridge]` expander: walks the inner mod, splits wasm
// stub vs native impl emission, and wires the cardinality marker.
// The pieces share captured spans / item refs so factoring out helpers
// would force ad-hoc shared-state structs that read worse than a
// linear walk.
#[allow(clippy::too_many_lines)]
fn expand_bridge(mut item_mod: ItemMod, opts: BridgeOpts) -> syn::Result<TokenStream2> {
    let BridgeOpts {
        cardinality,
        feature,
        one_per,
    } = opts;
    // `one_per` only describes an instanced family's entity relationship.
    // On a singleton (or a bare `#[bridge]`) it is meaningless — reject it
    // rather than silently dropping it. Validated here (not in the parser)
    // because attribute arg order is unspecified, so `cardinality` may not
    // be known yet when `one_per` is seen.
    if one_per.is_some() && !matches!(cardinality, Some(BridgeCardinality::Instanced)) {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[bridge] `one_per` requires `instanced` — it declares the entity \
             relationship of an instanced family (ADR-0088 §4 v2)",
        ));
    }
    let Some((brace, items)) = item_mod.content.take() else {
        return Err(syn::Error::new_spanned(
            &item_mod,
            "#[bridge] must be applied to an inline `mod foo { ... }` block, not a file-backed mod",
        ));
    };
    let mod_ident = item_mod.ident.clone();

    // Find the `#[actor] impl NativeActor for X { ... }` block.
    // Collect its index (so we can rewrite the attribute in place) and
    // walk its items to extract X, NAMESPACE, and handler kinds.
    let mut actor_idx: Option<usize> = None;
    for (idx, it) in items.iter().enumerate() {
        if let Item::Impl(impl_block) = it
            && impl_block.attrs.iter().any(attr_is_actor)
        {
            actor_idx = Some(idx);
            break;
        }
    }
    let Some(actor_idx) = actor_idx else {
        return Err(syn::Error::new_spanned(
            &item_mod,
            "#[bridge] expects exactly one `#[actor] impl NativeActor for X { ... }` block \
             inside the wrapped mod",
        ));
    };

    // Collect everything we need from the inner actor impl into owned
    // values so the borrow on `items` ends before we mutate it below.
    let (self_ty, type_ident, generics, namespace_expr, handler_kinds, catch_all) = {
        let Item::Impl(actor_impl) = &items[actor_idx] else {
            unreachable!("actor_idx points to an Item::Impl by construction");
        };

        // Reject `impl Trait for X` shapes that aren't `NativeActor`.
        let trait_path = actor_impl.trait_.as_ref().ok_or_else(|| {
            syn::Error::new_spanned(
                actor_impl,
                "#[bridge]'s inner #[actor] block must be `impl NativeActor for X`, \
                 not an inherent `impl X { ... }`",
            )
        })?;
        let last_seg = trait_path
            .1
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        if last_seg != "NativeActor" {
            return Err(syn::Error::new_spanned(
                &trait_path.1,
                format!(
                    "#[bridge]'s inner #[actor] block must be `impl NativeActor for X` — got `{last_seg}`",
                ),
            ));
        }

        // Pull the bare struct ident out of `self_ty`. Required for the
        // wasm-stub + re-export emission, where we need to write
        // `pub struct X;` and `pub use <mod>::X;` referencing X by name.
        let type_ident = match &*actor_impl.self_ty {
            Type::Path(tp) if tp.qself.is_none() && tp.path.segments.len() == 1 => {
                tp.path.segments[0].ident.clone()
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &actor_impl.self_ty,
                    "#[bridge]'s actor type must be a bare ident (`HandleCapability`), not a \
                     path or generic — the macro emits a wasm-stub `pub struct X;` and a \
                     `pub use <mod>::X;` re-export referencing it by name",
                ));
            }
        };

        // Walk the impl items to collect handler kind types and the
        // `NAMESPACE` const expression. The const lives on the supertrait
        // `Actor`, but the user wrote it inside `impl NativeActor for X`
        // — same source-of-truth contract `expand_native_actor_trait`
        // uses.
        let mut handler_kinds: Vec<Type> = Vec::new();
        let mut has_fallback = false;
        let mut namespace_expr: Option<Expr> = None;
        for impl_item in &actor_impl.items {
            match impl_item {
                ImplItem::Fn(f) if f.attrs.iter().any(attr_is_handler) => {
                    // ADR-0093 §3: `#[handler(task)]` completions route by
                    // their `TaskDone<O, C>` output type, not a kind id —
                    // they carry no mail kind and emit no `HandlesKind`
                    // marker, so skip them when collecting the mail-handler
                    // kinds. Only `#[handler]` / `#[handler(mail)]` feed the
                    // marker set.
                    let handler_attr = f
                        .attrs
                        .iter()
                        .find(|a| attr_is_handler(a))
                        .expect("matched on attr_is_handler above");
                    if parse_handler_variant(handler_attr)? == HandlerVariant::Task {
                        continue;
                    }
                    let (kind_ty, _is_slice) = extract_native_actor_handler_kind(&f.sig)?;
                    handler_kinds.push(kind_ty);
                }
                ImplItem::Fn(f) if f.attrs.iter().any(attr_is_fallback) => {
                    has_fallback = true;
                }
                ImplItem::Const(c) => {
                    if c.ident == "NAMESPACE" {
                        namespace_expr = Some(c.expr.clone());
                    } else if c.ident == "SCHEDULING" {
                        // Issue 1187: dispatch placement is no longer
                        // authorable — the scheduling enum + trait const
                        // were removed. Reject a leftover const with a
                        // pointed diagnostic rather than letting it fall
                        // through to a surfaceless-trait error.
                        return Err(syn::Error::new_spanned(
                            c,
                            "`SCHEDULING` was removed (issue 1187): every actor drains on the \
                             chassis worker pool. Drop the const — never block a handler; \
                             offload blocking work to a `ctx.spawn`'d thread that feeds results \
                             back as mail.",
                        ));
                    }
                }
                _ => {}
            }
        }

        let namespace_expr = namespace_expr.ok_or_else(|| {
            syn::Error::new_spanned(
                actor_impl,
                "#[bridge]'s inner #[actor] block must declare \
                 `const NAMESPACE: &'static str = ...` so the marker `impl Actor` can carry it",
            )
        })?;
        // Issue 576 + issue 603: bridge-wrapped actors come in three
        // flavours — strict typed receiver (only `#[handler]`s),
        // catch-all cap (only `#[fallback]`), or hybrid (typed
        // handlers + a `#[fallback]` runtime safety net). The hybrid
        // shape is what `ComponentHostCapability` uses: declared kinds
        // it accepts are typed; unknown kinds (Phase 1 chassis-
        // peripheral migration window) ride the fallback. Hybrid emits
        // per-handler `HandlesKind<K>` impls (no blanket — declared
        // kinds compile, undeclared do not), so the type-system
        // strictness is unchanged from a pure-handler cap.
        if handler_kinds.is_empty() && !has_fallback {
            return Err(syn::Error::new_spanned(
                actor_impl,
                "#[bridge]'s inner #[actor] block must declare at least one #[handler] method \
                 or a #[fallback] method",
            ));
        }
        (
            (*actor_impl.self_ty).clone(),
            type_ident,
            actor_impl.generics.clone(),
            namespace_expr,
            handler_kinds,
            has_fallback,
        )
    };

    let (impl_generics, _, where_clause) = generics.split_for_impl();

    // Always-on marker surface, emitted as siblings of the mod.
    //
    // - On wasm, the real struct (inside `mod native`) is cfg-stripped;
    //   a unit-struct stub takes its place at file root so the marker
    //   impls below have a type to reference. Wasm consumers never
    //   construct caps — they only address them by type via
    //   `ctx.actor::<X>().send(&kind)` — so the stub being uninhabited
    //   is fine.
    // - On native, the `pub use` re-exports the real struct from `mod
    //   native` to file root, so callers writing `crate::log::LogCapability`
    //   (chassis builders, tests in sibling mods) keep working.
    // - Singleton, Actor, and HandlesKind impls are always-on so wasm
    //   consumers compile typed sends without the substrate runtime.
    // When the bridge declares `feature = "X"`, the wasm stub also
    // covers "native target without the feature" so consumers that
    // build with `default-features = false` keep a reachable type for
    // the always-on markers below. The native re-export and the inner
    // `mod native` then key on `feature = "X"` too.
    let (stub_cfg, native_cfg) = match feature.as_deref() {
        None => (
            quote! { #[cfg(target_arch = "wasm32")] },
            quote! { #[cfg(not(target_arch = "wasm32"))] },
        ),
        Some(feat) => (
            quote! { #[cfg(any(target_arch = "wasm32", not(feature = #feat)))] },
            quote! { #[cfg(all(not(target_arch = "wasm32"), feature = #feat))] },
        ),
    };
    let stub_and_reexport = quote! {
        #stub_cfg
        pub struct #type_ident;

        #native_cfg
        pub use #mod_ident::#type_ident;
    };
    // Issue 625: cardinality is an explicit declaration on the bridge
    // attribute per ADR-0079. The bridge sits over two struct
    // definition sites — the wasm stub at file root and the native
    // struct inside `mod native` — and the cardinality marker impl
    // must land at file root to cover both. The author writes
    // `#[bridge(singleton)]` or `#[bridge(instanced)]`; absence is
    // hand-rolled (test fixtures, future cases that don't fit either).
    let actor_marker = quote! {
        impl #impl_generics ::aether_actor::Actor for #self_ty #where_clause {
            const NAMESPACE: &'static str = #namespace_expr;
        }
    };
    // The cardinality marker (issue 625 / ADR-0079) plus its ADR-0088
    // §3/§4 reverse-lookup submission. The bridge already knows the
    // actor's `NAMESPACE` and cardinality, so it auto-submits the
    // name-inventory entry next to the marker — no per-actor registration
    // list to keep in sync (the drift hazard iamacoffeepot/aether#1036
    // flags). A **singleton**'s `NAMESPACE` *is* its mailbox name, so it
    // submits a `NameEntry` (the static reverse map folds it, letting a
    // `MailboxId` reverse to `aether.audio` instead of a hex tag). An
    // **instanced** actor's `NAMESPACE` is the prefix of
    // `<NAMESPACE>:<subname>` instances, so it submits a `Dynamic`
    // `TemplateEntry` — the family's shape is declared, individual
    // instances reverse via the runtime registry. (Typed instance
    // parameters are future work; for now the convention is a single
    // `:<subname>` string hole.) ADR-0088 §4 v2 adds the orthogonal
    // `cardinality`: `one_per = "entity"` ⇒ `Cardinality::OnePer(entity)`
    // (the relationship every instanced actor actually has — one per
    // component / connection / …), absent ⇒ `Cardinality::Unbounded`.
    // Both submissions are gated by the same `native_cfg` as the rest of
    // the native surface (the `inventory` crate doesn't link on wasm32,
    // and a feature-gated cap's entry tracks the cap's availability).
    let instanced_cardinality = if let Some(entity) = one_per.as_deref() {
        let entity = syn::LitStr::new(entity, proc_macro2::Span::call_site());
        quote! { ::aether_data::name_inventory::Cardinality::OnePer(#entity) }
    } else {
        quote! { ::aether_data::name_inventory::Cardinality::Unbounded }
    };
    let cardinality_marker = match cardinality {
        Some(BridgeCardinality::Singleton) => quote! {
            impl #impl_generics ::aether_actor::Singleton for #self_ty #where_clause {}
            #native_cfg
            ::aether_data::name_inventory::inventory::submit! {
                ::aether_data::name_inventory::NameEntry {
                    domain: ::aether_data::MAILBOX_DOMAIN,
                    name: #namespace_expr,
                }
            }
        },
        Some(BridgeCardinality::Instanced) => quote! {
            impl #impl_generics ::aether_actor::Instanced for #self_ty #where_clause {}
            #native_cfg
            ::aether_data::name_inventory::inventory::submit! {
                ::aether_data::name_inventory::TemplateEntry {
                    domain: ::aether_data::MAILBOX_DOMAIN,
                    // Split the namespace (prefix) from the structural
                    // `:{subname}` suffix so a forward-fed const NAMESPACE
                    // (e.g. `EmbeddedHost::NAMESPACE`, ADR-0099 §5/§6) works —
                    // `concat!` would reject a non-literal namespace.
                    prefix: #namespace_expr,
                    template: ":{subname}",
                    param: ::aether_data::name_inventory::ParamKind::Dynamic,
                    cardinality: #instanced_cardinality,
                }
            }
        },
        None => quote! {},
    };
    // Issue 576 + issue 603: only-fallback (true catch-all) caps emit
    // one blanket `impl<K: Kind> HandlesKind<K> for X {}` so typed
    // sends compile for every K. Strict receivers (only `#[handler]`s)
    // and hybrid caps (handlers + fallback) emit per-handler impls so
    // only declared kinds are reachable via `ctx.actor::<X>().send`.
    // The fallback in the hybrid shape is a runtime safety net for
    // mail that arrives by mailbox name, not a type-system catch-all.
    let only_fallback = catch_all && handler_kinds.is_empty();
    let handles_kind_markers: Vec<TokenStream2> = if only_fallback {
        let kind_param: syn::Ident = syn::parse_quote!(__AetherCatchAllK);
        let mut blanket_generics = generics.clone();
        blanket_generics.params.push(syn::parse_quote!(
            #kind_param: ::aether_actor::__macro_internals::Kind
        ));
        let (blanket_impl, _, blanket_where) = blanket_generics.split_for_impl();
        vec![quote! {
            impl #blanket_impl ::aether_actor::HandlesKind<#kind_param>
                for #self_ty #blanket_where {}
        }]
    } else {
        handler_kinds
            .iter()
            .map(|kind_ty| {
                quote! {
                    impl #impl_generics ::aether_actor::HandlesKind<#kind_ty>
                        for #self_ty #where_clause {}
                }
            })
            .collect()
    };

    // Rewrite the inner `#[actor]` to `#[actor(skip_markers)]` so the
    // expander inside the cfg-gated mod doesn't duplicate the markers
    // we just emitted. Preserve the user's original path token so a
    // `use aether_actor::actor;` line remains "used" — replacing with
    // an absolute path would silently produce unused-import warnings
    // in caller code.
    let mut items = items;
    if let Item::Impl(actor_impl_mut) = &mut items[actor_idx] {
        for attr in &mut actor_impl_mut.attrs {
            if attr_is_actor(attr) {
                let path = attr.path().clone();
                *attr = syn::parse_quote!(#[#path(skip_markers)]);
            }
        }
    }

    // Reassemble the mod with the rewritten contents and prepend the
    // cfg gate. `native_cfg` keys on the optional feature too — without
    // a feature it's the original `not(target_arch = "wasm32")`; with
    // one it adds `feature = "X"` so the inner `mod native` only
    // compiles when both the target and the feature say to.
    item_mod.content = Some((brace, items));
    let mod_attrs = mem::take(&mut item_mod.attrs);
    Ok(quote! {
        #stub_and_reexport
        #actor_marker
        #cardinality_marker
        #(#handles_kind_markers)*

        #(#mod_attrs)*
        #native_cfg
        #item_mod
    })
}

/// Match `#[actor]` or `#[crate::actor]` or `#[aether_actor::actor]` —
/// any path whose last segment is `actor`.
fn attr_is_actor(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "actor")
}

#[proc_macro_attribute]
pub fn handler(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    // Real logic runs inside `#[actor]` (the enclosing impl-block
    // attribute scans for #[handler] markers). This standalone shim
    // only exists so rustc accepts `#[handler]` syntactically outside
    // macro expansion and so rust-analyzer doesn't redline it.
    syn::Error::new(
        proc_macro2::Span::call_site(),
        "#[handler] may only appear inside a `#[actor] impl FfiActor for T` block",
    )
    .to_compile_error()
    .into()
}

#[proc_macro_attribute]
pub fn fallback(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    // Same story as `#[handler]` — marker attribute consumed by the
    // enclosing `#[actor]` scan. Standalone invocation is a
    // compile-time error.
    syn::Error::new(
        proc_macro2::Span::call_site(),
        "#[fallback] may only appear inside a `#[actor] impl FfiActor for T` block",
    )
    .to_compile_error()
    .into()
}

/// `#[local]` — attribute macro that declares a struct as
/// per-actor scratch storage (issue 582). Passes the struct
/// through unchanged and emits `impl ::aether_actor::Local for T
/// {}` underneath.
///
/// The trait requires `Default + Send + 'static` (native) /
/// `Default + 'static` (wasm) — the user supplies `Default` either
/// via `#[derive(Default)]` or a hand-rolled impl, depending on
/// whether the struct's fields default trivially. The macro
/// deliberately does *not* auto-derive `Default` so types that
/// need a custom default (e.g. a counter that starts at 1, a Vec
/// with reserved capacity) aren't fighting the derive.
///
/// ```ignore
/// #[derive(Default)]
/// #[local]
/// struct LogBuffer(Vec<LogEvent>);
///
/// #[derive(Default)]
/// #[local]
/// struct AppState {
///     pending: u32,
///     events: Vec<Event>,
/// }
///
/// // Custom Default:
/// #[local]
/// struct Retries { count: u32 }
/// impl Default for Retries {
///     fn default() -> Self { Self { count: 3 } }
/// }
/// ```
///
/// Generics are forwarded — `#[local] struct Foo<T>(T);` emits
/// `impl<T: Default + Send + 'static> Local for Foo<T>`. In
/// practice Local types are concrete; the generics support is
/// mostly for completeness.
#[proc_macro_attribute]
pub fn local(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::ItemStruct);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    quote! {
        #input
        impl #impl_generics ::aether_actor::Local for #name #ty_generics #where_clause {}
    }
    .into()
}

/// `#[derive(Singleton)]` — emits `impl ::aether_actor::Singleton for T {}`.
///
/// Per ADR-0079 (issue 607) cardinality is first-class:
/// [`Singleton`] and [`Instanced`] are mutually exclusive at the type
/// level. Issue 625 made the choice explicit at the struct
/// definition rather than auto-emitted by `#[bridge]` — `Singleton`
/// is a property of the type, not of any one trait it implements.
/// Authors place `#[derive(Singleton)]` on the cap struct alongside
/// `pub struct X` inside the bridge mod; absence selects the other
/// cardinality (and the type-system catches mistakes — `Builder::with_actor`
/// requires `Singleton`, `ctx.spawn_child` requires `Instanced`).
#[proc_macro_derive(Singleton)]
pub fn derive_singleton(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    quote! {
        impl #impl_generics ::aether_actor::Singleton for #name #ty_generics #where_clause {}
    }
    .into()
}

/// `#[derive(Instanced)]` — emits `impl ::aether_actor::Instanced for T {}`.
///
/// The instanced counterpart of [`derive_singleton`]. Per ADR-0079
/// (issue 607), instanced actors carry a runtime subname under their
/// `NAMESPACE` prefix — full names hash to `"{NAMESPACE}:{subname}"`
/// (e.g. `aether.tcp.listener:8080`). Authors place
/// `#[derive(Instanced)]` on the cap struct inside the bridge mod;
/// `Builder::with_actor` rejects instanced types at compile time
/// (the chassis-builder boots singletons only), and
/// `ctx.spawn_child` requires the `Instanced` bound.
#[proc_macro_derive(Instanced)]
pub fn derive_instanced(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    quote! {
        impl #impl_generics ::aether_actor::Instanced for #name #ty_generics #where_clause {}
    }
    .into()
}

/// `#[derive(Embeddable)]` — marks an **embeddable** actor: an FFI/wasm
/// component reached by peers as `ctx.actor::<T>()`. Emits the `Singleton`
/// marker with a `resolve` override that **delegates to the embedding-host
/// class** (ADR-0099 §5/§6) instead of the depth-1 default:
///
/// ```ignore
/// impl Singleton for T {
///     fn resolve(_caller_carry: u64) -> MailboxId {
///         ::aether_capabilities::resolve_embedded(<T as Actor>::NAMESPACE)
///     }
/// }
/// ```
///
/// The override ignores the caller's carry — an embeddable component's
/// address is absolute, rooted at the component host, not relative to the
/// caller. It folds the component's own `NAMESPACE` as an instance under
/// the reserved `aether.embedded` class namespace onto the host cap's
/// carry, landing on the registered mailbox a bare-`NAMESPACE` hash would
/// miss (iamacoffeepot/aether#1364).
///
/// The macro writes **no namespace literal** — it emits a *call* to
/// `resolve_embedded`, which reads `aether.embedded` only inside its
/// owner (`aether_actor::EmbeddedHost`) and the `aether.component` host
/// carry only from `ComponentHostCapability`. The
/// only string the author writes is `T`'s own `NAMESPACE` (ADR-0099 §5's
/// read-from-owner rule). Because the emitted path is
/// `::aether_capabilities::resolve_embedded`, a peer-addressable
/// embeddable depends on `aether-capabilities` (as `aether-kit` already
/// does). Use in place of `#[derive(Singleton)]`, not alongside it.
#[proc_macro_derive(Embeddable)]
pub fn derive_embeddable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    quote! {
        impl #impl_generics ::aether_actor::Singleton for #name #ty_generics #where_clause {
            fn resolve(_caller_carry: u64) -> ::aether_data::MailboxId {
                ::aether_capabilities::resolve_embedded(
                    <#name #ty_generics as ::aether_actor::Actor>::NAMESPACE,
                )
            }
        }
    }
    .into()
}

/// `#[capability]` — attribute macro for native chassis capability
/// structs. Cfg-gates every field with `#[cfg(feature = "native")]`
/// so the cap's runtime fields disappear from non-native builds (wasm
/// guests linking the cap's depable rlib for type/marker visibility
/// don't pay for `cpal::Stream`, `Arc<HandleStore>`, etc.).
///
/// Issue 552 stage 0 ships the macro as a thin shim — fields get the
/// blanket `#[cfg(feature = "native")]` gate and the struct itself
/// passes through unchanged. Stage 1 may extend the macro to gate
/// trait impls, derive `Default`, or pre-emit the empty
/// stage-0-required `Singleton` marker; this skeleton lands now so
/// capability authors can adopt the new shape without waiting on
/// stage 1 details.
///
/// ```ignore
/// #[capability]
/// pub struct AudioCapability {
///     // Both fields gain `#[cfg(feature = "native")]` automatically.
///     audio_sender: Option<AudioEventSender>,
///     audio_thread: Option<JoinHandle<()>>,
/// }
/// ```
#[proc_macro_attribute]
pub fn capability(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[capability] takes no arguments",
        )
        .to_compile_error()
        .into();
    }
    let mut item = parse_macro_input!(item as syn::ItemStruct);
    // Issue 552 stage 4: gate fields on `not(target_arch = "wasm32")`
    // to match the macro-emitted `NativeActor` / `NativeDispatch`
    // impls. Wasm builds see the cap struct with no fields (a pure
    // marker), which is what typed `ctx.actor::<R>().send(...)` needs;
    // host builds see the full struct.
    match &mut item.fields {
        Fields::Named(fields) => {
            for field in &mut fields.named {
                let already_cfg = field.attrs.iter().any(|a| a.path().is_ident("cfg"));
                if !already_cfg {
                    field
                        .attrs
                        .push(syn::parse_quote!(#[cfg(not(target_arch = "wasm32"))]));
                }
            }
        }
        Fields::Unnamed(fields) => {
            for field in &mut fields.unnamed {
                let already_cfg = field.attrs.iter().any(|a| a.path().is_ident("cfg"));
                if !already_cfg {
                    field
                        .attrs
                        .push(syn::parse_quote!(#[cfg(not(target_arch = "wasm32"))]));
                }
            }
        }
        Fields::Unit => {
            // Marker structs: nothing to gate.
        }
    }
    quote! { #item }.into()
}

struct HandlerFn {
    method: syn::ImplItemFn,
    kind_ty: Type,
    agent_doc: Option<String>,
    /// ADR-0109: the handler's reply contract, classified from its
    /// return type. Drives the auto-emitted `ctx.reply` and the reply
    /// kind id on the inputs manifest record.
    reply: HandlerReply,
    /// ADR-0112: the declared reply class (single / manual). Selects the
    /// ctx view the macro passes (`as_single()` for single, the full
    /// `Manual` ctx for manual) and the manifest `ReplyContract` tag.
    class: HandlerClass,
}

struct FallbackFn {
    method: syn::ImplItemFn,
    agent_doc: Option<String>,
}

fn expand_handlers(item: ItemImpl, opts: ActorOpts) -> syn::Result<TokenStream2> {
    if let Some((_, trait_path, _)) = item.trait_.as_ref() {
        // Pattern-match the trait path's last identifier so the macro
        // works regardless of the user's import style — bare
        // `FfiActor` / `NativeActor`, `aether_actor::FfiActor`,
        // `aether_substrate::NativeActor`, etc. all resolve here.
        let last = trait_path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        match last.as_str() {
            "NativeActor" => expand_native_actor_trait(item, opts),
            // `FfiActor` is the post-552 trait name; `Component` is
            // the back-compat alias retained until stage 4.
            "FfiActor" | "Component" => {
                if opts.skip_markers {
                    return Err(syn::Error::new_spanned(
                        trait_path,
                        "#[actor(skip_markers)] is only meaningful on \
                         `impl NativeActor for X` blocks wrapped by `#[bridge]`",
                    ));
                }
                expand_wasm_actor(item)
            }
            other => Err(syn::Error::new_spanned(
                trait_path,
                format!(
                    "#[actor] expects `impl FfiActor for X`, `impl NativeActor for X`, or \
                     `impl Component for X` (back-compat alias) — got `{other}`",
                ),
            )),
        }
    } else {
        // Inherent `impl X { … }` is rejected — every native chassis cap
        // now goes through `#[actor] impl NativeActor for X`. Pre-issue-688
        // this arm emitted `impl Dispatch for X` for the legacy
        // `Builder::with(cap)` facade path; that path retired alongside
        // the `Dispatch` trait itself.
        Err(syn::Error::new_spanned(
            &item.self_ty,
            "#[actor] expects `impl FfiActor for X`, `impl NativeActor for X`, or \
             `impl Component for X` (back-compat alias) — inherent `impl X { … }` \
             is no longer supported",
        ))
    }
}

/// Match a handler attribute — bare `#[handler]` (any path whose last
/// segment is `handler`, so `#[crate::handler]` / `#[aether_data::handler]`
/// resolve too) or a class-marked `#[handler::single|manual|stream]`
/// (ADR-0112), whose last segment is the class and whose preceding
/// segment is `handler`. The class path never reaches attribute
/// resolution — `#[actor]` parses and strips it.
fn attr_is_handler(attr: &Attribute) -> bool {
    let segments = &attr.path().segments;
    let Some(last) = segments.last() else {
        return false;
    };
    if last.ident == "handler" {
        return true;
    }
    if matches!(
        last.ident.to_string().as_str(),
        "single" | "manual" | "stream"
    ) {
        let len = segments.len();
        return len >= 2 && segments[len - 2].ident == "handler";
    }
    false
}

/// Same logic for `#[fallback]`.
fn attr_is_fallback(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "fallback")
}

/// The category of a `#[handler]` method (ADR-0093 §3). `#[handler]` and
/// `#[handler(mail)]` both mean an inbound-mail handler (the default);
/// `#[handler(task)]` marks a hold-until-resolve dispatch completion,
/// matched by its `TaskDone<O, C>` output type rather than a kind id.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandlerVariant {
    Mail,
    Task,
}

/// Parse the parenthesized argument of a `#[handler(...)]` attribute into
/// a [`HandlerVariant`]. Bare `#[handler]` (no parens) is `Mail`. The
/// only accepted parenthesized spellings are `mail` and `task`; anything
/// else is a pointed compile error spanned at the attribute.
fn parse_handler_variant(attr: &Attribute) -> syn::Result<HandlerVariant> {
    match &attr.meta {
        // Bare `#[handler]` — the default inbound-mail handler.
        Meta::Path(_) => Ok(HandlerVariant::Mail),
        // `#[handler(mail)]` / `#[handler(task)]` — parse the single
        // ident argument.
        Meta::List(_) => {
            let ident: syn::Ident = attr.parse_args().map_err(|_| {
                syn::Error::new_spanned(
                    attr,
                    "#[handler(...)] accepts exactly `mail` or `task` — \
                     `#[handler]` and `#[handler(mail)]` are inbound mail, \
                     `#[handler(task)]` is a dispatch completion (ADR-0093 §3)",
                )
            })?;
            if ident == "mail" {
                Ok(HandlerVariant::Mail)
            } else if ident == "task" {
                Ok(HandlerVariant::Task)
            } else {
                Err(syn::Error::new_spanned(
                    &ident,
                    "unknown #[handler] variant — accepts exactly `mail` or `task` \
                     (`#[handler]` / `#[handler(mail)]` = inbound mail, \
                     `#[handler(task)]` = a dispatch completion, ADR-0093 §3)",
                ))
            }
        }
        Meta::NameValue(nv) => Err(syn::Error::new_spanned(
            nv,
            "#[handler] takes no `= value` — write `#[handler]`, `#[handler(mail)]`, \
             or `#[handler(task)]`",
        )),
    }
}

/// The reply class of a handler (ADR-0112), read off the attribute path:
/// `#[handler]` / `#[handler::single]` are [`Single`](HandlerClass::Single),
/// `#[handler::manual]` is [`Manual`](HandlerClass::Manual), and
/// `#[handler::stream]` is [`Stream`](HandlerClass::Stream) — reserved,
/// rejected by [`parse_handler_class`]. Orthogonal to [`HandlerVariant`]
/// (the `mail` / `task` trigger), which is read from the parens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandlerClass {
    Single,
    Manual,
    Stream,
}

/// Read a handler's [`HandlerClass`] off its attribute path (ADR-0112).
/// The last path segment is the class (`single` / `manual` / `stream`),
/// or `handler` itself for the bare `#[handler]` (= single). `stream` is
/// a hard error — the class is reserved and its emit surface isn't built.
/// `attr_is_handler` is the gate, so the path is known to end in one of
/// these segments.
fn parse_handler_class(attr: &Attribute) -> syn::Result<HandlerClass> {
    let last = attr
        .path()
        .segments
        .last()
        .expect("attr_is_handler guarantees a non-empty path");
    let class = match last.ident.to_string().as_str() {
        // Bare `#[handler]` / `#[handler(mail|task)]` and the explicit
        // `#[handler::single]` are both the single class.
        "handler" | "single" => HandlerClass::Single,
        "manual" => HandlerClass::Manual,
        "stream" => HandlerClass::Stream,
        other => {
            return Err(syn::Error::new_spanned(
                attr,
                format!(
                    "unknown #[handler::<class>] — accepts `single`, `manual`, or `stream` \
                     (ADR-0112); got `{other}`"
                ),
            ));
        }
    };
    if class == HandlerClass::Stream {
        return Err(syn::Error::new_spanned(
            attr,
            "#[handler::stream] is reserved and not yet implemented (ADR-0112)",
        ));
    }
    Ok(class)
}

/// Extract `(O, C, is_borrow)` from a `#[handler(task)]` method's third
/// parameter, which must be `done: TaskDone<O>` (where `C` defaults to
/// `()`) or `done: TaskDone<O, C>`, optionally behind a shared `&`.
/// Unlike a mail handler's third parameter (a `Kind`), a task
/// completion's parameter is the framework's `TaskDone<...>` — `O` / `C`
/// are its generic arguments, not a kind. `is_borrow` is `true` when the
/// parameter is `&TaskDone<…>` (the ADR-0109 opt-in for a macro-driven
/// reply) versus the by-value `TaskDone<…>` self-resolve form.
fn extract_task_handler_types(sig: &Signature) -> syn::Result<(Type, Type, bool)> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[handler(task)] method must have signature \
             `(&self | &mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<O>)` \
             (or `TaskDone<O, C>` with an opt-in context)",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(
            first,
            "#[handler(task)] first parameter must be `&self` or `&mut self`",
        ));
    }
    let third = &sig.inputs[2];
    let FnArg::Typed(pt) = third else {
        return Err(syn::Error::new_spanned(
            third,
            "#[handler(task)] third parameter must be `done: TaskDone<O>` or `TaskDone<O, C>`",
        ));
    };
    // ADR-0109: `&TaskDone<…>` (the macro-driven reply opt-in) vs the
    // by-value `TaskDone<…>` self-resolve form. Peel a leading shared
    // reference and remember which shape it was.
    let (is_borrow, inner_ty): (bool, &Type) = match &*pt.ty {
        Type::Reference(r) => (true, &*r.elem),
        other => (false, other),
    };
    let Type::Path(type_path) = inner_ty else {
        return Err(syn::Error::new_spanned(
            &pt.ty,
            "#[handler(task)] third parameter must be a `TaskDone<O>` / `TaskDone<O, C>` path type \
             (optionally behind `&`)",
        ));
    };
    let last = type_path.path.segments.last().ok_or_else(|| {
        syn::Error::new_spanned(
            &pt.ty,
            "#[handler(task)] third parameter must be `TaskDone<…>`",
        )
    })?;
    if last.ident != "TaskDone" {
        return Err(syn::Error::new_spanned(
            &pt.ty,
            "#[handler(task)] third parameter must be `TaskDone<O>` or `TaskDone<O, C>` \
             (the framework completion type, ADR-0093 §3)",
        ));
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return Err(syn::Error::new_spanned(
            last,
            "#[handler(task)] `TaskDone` needs an output type argument: `TaskDone<O>` or \
             `TaskDone<O, C>`",
        ));
    };
    let type_args: Vec<&Type> = args
        .args
        .iter()
        .filter_map(|a| match a {
            GenericArgument::Type(t) => Some(t),
            _ => None,
        })
        .collect();
    let output = match type_args.first() {
        Some(t) => (*t).clone(),
        None => {
            return Err(syn::Error::new_spanned(
                last,
                "#[handler(task)] `TaskDone` needs an output type argument: `TaskDone<O>`",
            ));
        }
    };
    // `C` defaults to `()` (a bare `TaskDone<O>` / `dispatch_blocking`).
    let context = type_args
        .get(1)
        .map_or_else(|| syn::parse_quote!(()), |t| (*t).clone());
    if type_args.len() > 2 {
        return Err(syn::Error::new_spanned(
            last,
            "#[handler(task)] `TaskDone` takes at most two type arguments: `TaskDone<O, C>`",
        ));
    }
    Ok((output, context, is_borrow))
}

/// How a `#[handler(task)]` completion discharges its reply (ADR-0109),
/// classified from its third-parameter borrow-ness plus its return type.
/// The `&TaskDone` borrow is the opt-in signal for a macro-driven reply.
enum TaskReplyMode {
    /// `TaskDone<O, C>` by value, `-> ()`: the handler owns the
    /// completion and calls `done.resolve*` itself (the ADR-0093 path,
    /// untouched).
    ByValue,
    /// `&TaskDone<O, C>` returning `-> R`: the handler borrows the
    /// completion and returns the reply; the macro calls
    /// `done.resolve_value(ctx, &r)` and releases the hold.
    BorrowReply,
    /// `&TaskDone<O, C>` returning `-> ()`: the handler borrows the
    /// completion and replies nothing; the macro calls
    /// `done.release_no_reply()` (the sanctioned no-reply discharge).
    BorrowNoReply,
}

/// Classify a `#[handler(task)]` completion's reply discharge from its
/// third-parameter borrow-ness (`is_borrow`) and return type (ADR-0109).
/// A by-value `TaskDone` keeps the self-resolve path and must return
/// `()`; `&TaskDone -> R` sends `R` via `resolve_value`, `&TaskDone -> ()`
/// releases via `release_no_reply`. A task completion can't itself defer
/// (`-> Pending<R>`).
fn classify_task_reply_mode(sig: &Signature, is_borrow: bool) -> syn::Result<TaskReplyMode> {
    let reply = classify_handler_reply(&sig.output);
    if is_borrow {
        match reply {
            HandlerReply::Sync(_) => Ok(TaskReplyMode::BorrowReply),
            HandlerReply::None => Ok(TaskReplyMode::BorrowNoReply),
            HandlerReply::Deferred(_) => Err(syn::Error::new_spanned(
                &sig.output,
                "#[handler(task)] cannot return `Pending<R>` — a dispatch completion \
                 is the terminal reply. Return `R` (the macro sends it via \
                 `resolve_value`) or `()` (release without replying)",
            )),
        }
    } else {
        match reply {
            HandlerReply::None => Ok(TaskReplyMode::ByValue),
            HandlerReply::Sync(_) | HandlerReply::Deferred(_) => Err(syn::Error::new_spanned(
                &sig.output,
                "a by-value `TaskDone<…>` #[handler(task)] must return `()` and call \
                 `done.resolve*` itself; to have the macro send the reply, borrow it: \
                 `done: &TaskDone<…>` returning `-> R`",
            )),
        }
    }
}

/// Wasm-actor expansion — `#[actor] impl FfiActor for X` (or
/// the back-compat `impl Component for X`). Emits the full wasm
/// surface: dispatch table referencing `aether_actor::FfiCtx<'_>`,
/// init wrapper, `aether.kinds.inputs` manifest consts, kind retention
/// statics, plus the `HandlesKind<K>` and `Actor` impls common to both
/// shapes.
#[allow(clippy::too_many_lines)] // emits the full wasm-actor surface in one go
fn expand_wasm_actor(item: ItemImpl) -> syn::Result<TokenStream2> {
    let self_ty = &item.self_ty;
    let generics = &item.generics;
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let trait_path = item
        .trait_
        .as_ref()
        .map(|(_, p, _)| p)
        .expect("trait_ checked above");

    let component_doc = extract_agent_doc(&item.attrs);

    let mut init_method: Option<syn::ImplItemFn> = None;
    let mut lifecycle_methods: Vec<syn::ImplItemFn> = Vec::new();
    let mut handlers: Vec<HandlerFn> = Vec::new();
    let mut fallback: Option<FallbackFn> = None;
    let mut helpers: Vec<syn::ImplItemFn> = Vec::new();
    // Issue 525 Phase 1B: pass-through trait consts (today just
    // NAMESPACE) so each component declares them inside its
    // `#[actor] impl FfiActor for C` block alongside `init` /
    // `#[handler]` methods.
    let mut consts: Vec<syn::ImplItemConst> = Vec::new();
    // ADR-0090 (issue 1256): optional `type Config = …` declaration.
    // When omitted, the macro synthesizes `type Config = ();` so the
    // emitted `export!` shim can decode 0 config bytes via
    // `impl Kind for ()` and the user's `init` body stays 1-param.
    let mut config_type: Option<syn::ImplItemType> = None;
    // ADR-0113 (issue 1855): optional `type State = …` declaration plus
    // the `dehydrate` / `rehydrate` accessor pair. When `type State` is
    // declared the macro generates the `on_dehydrate` / `on_rehydrate`
    // hooks from these (snapshot via `dehydrate`, restore via
    // `rehydrate`); when omitted it synthesizes `type State = ();` so a
    // no-persistence actor is unchanged.
    let mut state_type: Option<syn::ImplItemType> = None;
    let mut dehydrate_accessor: Option<syn::ImplItemFn> = None;
    let mut rehydrate_accessor: Option<syn::ImplItemFn> = None;

    for impl_item in item.items {
        match impl_item {
            ImplItem::Type(it) if it.ident == "Kinds" => {
                return Err(syn::Error::new_spanned(
                    it,
                    "#[actor] synthesizes `type Kinds` from the #[handler] methods; remove this declaration",
                ));
            }
            ImplItem::Type(it) if it.ident == "Config" => {
                config_type = Some(it);
            }
            ImplItem::Type(it) if it.ident == "State" => {
                state_type = Some(it);
            }
            ImplItem::Const(c) => {
                consts.push(c);
            }
            ImplItem::Fn(mut f) => {
                let name = f.sig.ident.to_string();
                let handler_attr_idx = f.attrs.iter().position(attr_is_handler);
                let fallback_attr_idx = f.attrs.iter().position(attr_is_fallback);

                if handler_attr_idx.is_some() && fallback_attr_idx.is_some() {
                    return Err(syn::Error::new_spanned(
                        &f,
                        "method cannot be both #[handler] and #[fallback]",
                    ));
                }

                if let Some(idx) = handler_attr_idx {
                    // ADR-0093 §7: dispatch completions are native-only.
                    // `try_take_task_done` lives on `NativeCtx`; the
                    // wasm/bridge path has no umbrella-aware blocking
                    // dispatch yet. Reject `#[handler(task)]` here with a
                    // clear diagnostic rather than letting it expand into
                    // a guest dispatch table that can't satisfy it.
                    if parse_handler_variant(&f.attrs[idx])? == HandlerVariant::Task {
                        return Err(syn::Error::new_spanned(
                            &f,
                            "dispatch completions are native-only (ADR-0093 §7); \
                             `#[handler(task)]` is not supported in wasm components",
                        ));
                    }
                    let kind_ty = extract_handler_kind_type(&f.sig)?;
                    let agent_doc = extract_agent_doc(&f.attrs);
                    let reply = classify_handler_reply(&f.sig.output);
                    // ADR-0112: read the reply class off the marker path
                    // (rejects `#[handler::stream]`).
                    let class = parse_handler_class(&f.attrs[idx])?;
                    f.attrs.remove(idx);
                    handlers.push(HandlerFn {
                        method: f,
                        kind_ty,
                        agent_doc,
                        reply,
                        class,
                    });
                } else if let Some(idx) = fallback_attr_idx {
                    if fallback.is_some() {
                        return Err(syn::Error::new_spanned(
                            &f,
                            "at most one #[fallback] method per component",
                        ));
                    }
                    validate_fallback_sig(&f.sig)?;
                    let agent_doc = extract_agent_doc(&f.attrs);
                    f.attrs.remove(idx);
                    fallback = Some(FallbackFn {
                        method: f,
                        agent_doc,
                    });
                } else if name == "init" {
                    init_method = Some(f);
                } else if matches!(
                    name.as_str(),
                    "wire" | "unwire" | "on_dehydrate" | "on_rehydrate"
                ) {
                    lifecycle_methods.push(f);
                } else if name == "receive" {
                    return Err(syn::Error::new_spanned(
                        &f,
                        "#[actor] synthesizes `fn receive`; remove this definition",
                    ));
                } else if name == "dehydrate" {
                    // ADR-0113: the save-side accessor — `fn dehydrate(&self)
                    // -> Self::State`. Routed out of `helpers` so the macro
                    // can validate the `type State` XOR and lift it into the
                    // inherent impl where the generated `on_dehydrate` calls
                    // `self.dehydrate()`.
                    dehydrate_accessor = Some(f);
                } else if name == "rehydrate" {
                    // ADR-0113: the restore-side accessor — `fn rehydrate(&mut
                    // self, state: Self::State)`. The generated `on_rehydrate`
                    // calls `self.rehydrate(..)` with the decoded state.
                    rehydrate_accessor = Some(f);
                } else {
                    helpers.push(f);
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unexpected item in #[actor] impl (only fns and the synthesized `type Kinds` are allowed)",
                ));
            }
        }
    }

    let mut init_method = init_method.ok_or_else(|| {
        syn::Error::new_spanned(
            self_ty,
            "#[actor] requires `fn init(ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError>` \
             (or, with `type Config = T`, `fn init(config: T, ctx: &mut FfiInitCtx<'_>) -> …`)",
        )
    })?;

    if handlers.is_empty() && fallback.is_none() {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] requires at least one #[handler] method or a #[fallback] method",
        ));
    }

    // Two `#[handler]` methods that accept the same mail kind would emit
    // two `HandlesKind<K>` impls (a coherence error) plus a dead second
    // dispatch arm the first arm always shadows. Reject the duplicate at
    // compile time, spanned at the later handler. The macro has no type
    // resolution, so dedup is by token equality (`types_token_eq`), not
    // by resolved `KindId`.
    for (i, later) in handlers.iter().enumerate() {
        if let Some(earlier) = handlers[..i]
            .iter()
            .find(|earlier| types_token_eq(&earlier.kind_ty, &later.kind_ty))
        {
            let earlier_name = &earlier.method.sig.ident;
            let kind_ty = &later.kind_ty;
            return Err(syn::Error::new_spanned(
                &later.method.sig.ident,
                format!(
                    "two #[handler] methods accept the same mail kind `{}` (also on \
                     `{earlier_name}`) — each kind routes to exactly one handler. Give each \
                     handler a distinct kind.",
                    quote!(#kind_ty)
                ),
            ));
        }
    }

    // ADR-0113 (issue 1855): declarative persistence. `type State` plus
    // the `dehydrate` / `rehydrate` accessor pair generate the
    // `on_dehydrate` / `on_rehydrate` hooks, so they are mutually
    // exclusive with hand-written hooks and require each other. Validate
    // the XOR at the offending span before synthesizing / generating.
    let manual_state_hook = lifecycle_methods.iter().find(|m| {
        matches!(
            m.sig.ident.to_string().as_str(),
            "on_dehydrate" | "on_rehydrate"
        )
    });
    if let Some(state) = state_type.as_ref() {
        // (a) `type State` + a hand-written hook is contradictory — the
        // macro already generates the hook from the accessors.
        if let Some(hook) = manual_state_hook {
            return Err(syn::Error::new_spanned(
                hook,
                "#[actor] generates `on_dehydrate` / `on_rehydrate` from `type State` plus the \
                 `dehydrate` / `rehydrate` accessors (ADR-0113); remove the hand-written hook, \
                 or drop `type State` and the accessors to write the hooks by hand",
            ));
        }
        // (c) `type State` needs both accessors — a half-pair would leave
        // one generated hook with no method to call.
        if dehydrate_accessor.is_none() {
            return Err(syn::Error::new_spanned(
                state,
                "`type State` requires a `fn dehydrate(&self) -> Self::State` accessor \
                 (ADR-0113) — the macro snapshots state through it in the generated \
                 `on_dehydrate`",
            ));
        }
        if rehydrate_accessor.is_none() {
            return Err(syn::Error::new_spanned(
                state,
                "`type State` requires a `fn rehydrate(&mut self, state: Self::State)` accessor \
                 (ADR-0113) — the macro restores state through it in the generated \
                 `on_rehydrate`",
            ));
        }
    } else if let Some(accessor) = dehydrate_accessor.as_ref().or(rehydrate_accessor.as_ref()) {
        // (b) an accessor without `type State` has no kind to (de)serialize.
        return Err(syn::Error::new_spanned(
            accessor,
            "`dehydrate` / `rehydrate` are the ADR-0113 persistence accessors and require a \
             `type State = …` declaration; add it, or rename the method if it is an unrelated \
             helper",
        ));
    }

    // Mirror `synthesized_config_type`: synthesize `type State = ();`
    // when the author omitted it (gated on `state_type.is_some()` at
    // macro time, NOT on `State != ()` at runtime), so a no-persistence
    // actor keeps the default no-op hooks and pays nothing.
    let synthesized_state_type: Option<syn::ImplItemType> = if state_type.is_some() {
        None
    } else {
        Some(syn::parse_quote!(
            type State = ();
        ))
    };

    // ADR-0090 (issue 1256): the trait now takes `init(config: Self::Config,
    // ctx: &mut C)`. If the user declared `type Config = …`, leave their
    // init alone — they're expected to spell out the `config` param. If
    // they omitted it, the macro synthesizes `type Config = ();` AND
    // injects a `_config: ()` leading param so the user's pre-#1256 body
    // (`fn init(ctx: &mut FfiInitCtx<'_>) -> …`) keeps compiling. The emitted shim
    // always decodes `<Self as FfiActor>::Config` from bytes, so the
    // synthesized `_config: ()` path round-trips uniformly via
    // `impl Kind for ()`.
    let (synthesized_config_type, init_method_emitted) = if config_type.is_some() {
        // User declared the config type; trust their init signature.
        (None, init_method)
    } else {
        // Synthesize `type Config = ();` and inject a leading `_config: ()`
        // parameter into init's signature so the user's 1-arg body
        // still type-checks against the new trait shape.
        let synth: syn::ImplItemType = syn::parse_quote!(
            type Config = ();
        );
        let config_param: FnArg = syn::parse_quote!(_config: ());
        // Inject at the front of the typed inputs. The init signature
        // has no `self` receiver (FfiActor::init is associated, not a
        // method), so index 0 is the right slot.
        init_method.sig.inputs.insert(0, config_param);
        (Some(synth), init_method)
    };

    // Issue #403: the SDK no longer prepends `ctx.subscribe_input::<K>()`
    // calls to `init` for the substrate's six fixed input streams (Tick,
    // Key, KeyRelease, MouseMove, MouseButton, WindowSize). Pre-#403
    // those calls fired during `Component::instantiate` — i.e. *before*
    // `try_register_component` published the mailbox — and were rejected
    // by `validate_subscriber_mailbox`. The substrate now derives those
    // subscriptions from the component's `aether.kinds.inputs` manifest
    // post-register. The `Ctx::subscribe_input` runtime API is still
    // available for components that want to subscribe / unsubscribe at
    // runtime (e.g. conditional input streams).
    let wrapped_init = init_method_emitted;
    let dispatch_body = build_dispatch_body(&handlers, fallback.as_ref());

    let handler_methods_tokens = handlers.iter().map(|h| &h.method);
    let fallback_method_tokens = fallback.as_ref().map(|f| &f.method);
    let helper_methods_tokens = helpers.iter();

    // ADR-0090 (issue 1257): surface the component's declared boot-config
    // kind. The macro emits a `Config` inputs record + a config-kind
    // retention static ONLY when the user explicitly declared
    // `type Config` (the synthesized `= ()` case stays clean — gating on
    // `config_type.is_some()` at macro time, NOT on `Config != ()` at
    // runtime, keeps `aether.unit` out of every component's capability).
    let config_kind_ty: Option<&Type> = config_type.as_ref().map(|it| &it.ty);
    let inputs_manifest_consts = build_inputs_manifest_consts(
        &handlers,
        fallback.as_ref(),
        component_doc.as_ref(),
        config_kind_ty,
    );
    let kind_retention_statics =
        build_kinds_section_retention_statics(self_ty, &handlers, config_kind_ty);

    // Issue 525 Phase 4: trait consts (today just NAMESPACE) live
    // on the `Actor` super-trait, not `Component` / `FfiActor`. Route
    // any const items the user declared inside `#[actor] impl
    // Component for X` to a sibling `impl ::aether_actor::Actor`
    // block so satisfying `FfiActor: Actor` works without making the
    // user split the impl manually.
    //
    // Validate the const surface first: `NAMESPACE` is required (the
    // marker `impl Actor` carries it) and is the only authorable const on
    // the `Actor` super-trait. A removed `SCHEDULING` const (issue 1187)
    // and any stray const are rejected at their own span, and a missing
    // `NAMESPACE` at the type — each a pointed diagnostic rather than a
    // later "no associated const NAMESPACE" error against the surfaceless
    // `Actor` trait.
    let mut has_namespace = false;
    for c in &consts {
        if c.ident == "NAMESPACE" {
            has_namespace = true;
        } else if c.ident == "SCHEDULING" {
            return Err(syn::Error::new_spanned(
                c,
                "`SCHEDULING` was removed (issue 1187): every actor drains on the chassis \
                 worker pool. Drop the const — never block a handler; offload blocking work \
                 to a `ctx.spawn`'d thread that feeds results back as mail.",
            ));
        } else {
            return Err(syn::Error::new_spanned(
                c,
                "#[actor] impl FfiActor for X accepts only \
                 `const NAMESPACE: &'static str = …` — the `Actor` super-trait carries no \
                 other authorable const",
            ));
        }
    }
    if !has_namespace {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] impl FfiActor for X must declare \
             `const NAMESPACE: &'static str = ...` so the marker `impl Actor` can carry it",
        ));
    }
    let const_tokens = consts.iter();
    let actor_impl = if consts.is_empty() {
        quote! {}
    } else {
        quote! {
            impl #impl_generics ::aether_actor::Actor for #self_ty #where_clause {
                #(#const_tokens)*
            }
        }
    };

    // ADR-0075: emit one `impl HandlesKind<K> for Self {}` per handler
    // kind. Auto-generated marker impls gate
    // `ActorMailbox<'_, R, T>::send::<K>` (constructed via
    // `ctx.actor::<R>()` / `ctx.resolve_actor::<R>(name)`) so wrong-kind
    // sends are compile errors at the call site. The handler list above
    // is the single source of truth — adding a `#[handler]` automatically
    // updates senders' compile-time checks.
    let handles_kind_impls = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        quote! {
            impl #impl_generics ::aether_actor::HandlesKind<#kind_ty>
                for #self_ty #where_clause {}
        }
    });

    // ADR-0090: emit the `type Config = …` line in the trait impl —
    // either the user's declaration (passed through) or the macro's
    // synthesized `type Config = ();`.
    let config_type_tokens = match (config_type.as_ref(), synthesized_config_type.as_ref()) {
        (Some(user), _) => quote! { #user },
        (None, Some(synth)) => quote! { #synth },
        (None, None) => unreachable!("synthesized_config_type is Some when user omitted"),
    };

    // ADR-0113: emit the `type State = …` line in the trait impl — the
    // user's declaration (passed through) or the synthesized `= ()`.
    let state_type_tokens = match (state_type.as_ref(), synthesized_state_type.as_ref()) {
        (Some(user), _) => quote! { #user },
        (None, Some(synth)) => quote! { #synth },
        (None, None) => unreachable!("synthesized_state_type is Some when user omitted"),
    };

    // ADR-0113: when the author declared `type State`, generate the
    // `on_dehydrate` / `on_rehydrate` hooks from the lifted accessors.
    // `Self::State` resolves directly inside `impl FfiActor for Self`.
    // `on_dehydrate` snapshots through `self.dehydrate()` and frames the
    // value with `save_state_kind`; `on_rehydrate` decodes via
    // `PriorState::as_kind` and either restores through `self.rehydrate`
    // or boots fresh, warning only when bytes were present but did not
    // decode (a reshaped state kind — `K::ID` changed). When `type State`
    // was omitted these are empty and the actor keeps the default no-op
    // hooks (or its own hand-written ones, carried in `lifecycle_methods`).
    let generated_state_hooks = if state_type.is_some() {
        quote! {
            fn on_dehydrate(&mut self, __aether_ctx: &mut ::aether_actor::FfiDropCtx<'_>) {
                let __aether_state = self.dehydrate();
                ::aether_actor::Persistence::save_state_kind::<
                    <Self as ::aether_actor::FfiActor>::State,
                >(__aether_ctx, 0, &__aether_state);
            }

            fn on_rehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::FfiCtx<'_>,
                __aether_prior: ::aether_actor::PriorState<'_>,
            ) {
                match __aether_prior.as_kind::<<Self as ::aether_actor::FfiActor>::State>() {
                    ::core::option::Option::Some(__aether_state) => {
                        self.rehydrate(__aether_state);
                    }
                    ::core::option::Option::None => {
                        if !__aether_prior.bytes().is_empty() {
                            ::aether_actor::__macro_internals::tracing::warn!(
                                "discarded prior state on rehydrate: bytes were present but did \
                                 not decode as the declared `type State` (a reshaped state kind); \
                                 booting fresh",
                            );
                        }
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    // ADR-0113: the lifted accessors ride as inherent methods on Self
    // (like handlers / helpers) so the generated trait-impl hooks can
    // call `self.dehydrate()` / `self.rehydrate(..)`. Both are `None`
    // when the actor declares no `type State`.
    let dehydrate_accessor_tokens = dehydrate_accessor.as_ref();
    let rehydrate_accessor_tokens = rehydrate_accessor.as_ref();

    // iamacoffeepot/aether#2048: the boot lifecycle (`init` / `wire` /
    // `unwire` + `type Config`) lives on the shared `Lifecycle` capability;
    // the hot-swap hooks (`on_dehydrate` / `on_rehydrate`) stay on the
    // target subtrait `FfiActor`. Route the user's hand-written hooks
    // accordingly — boot hooks into `impl Lifecycle`, hot-swap into
    // `impl FfiActor`. The per-target ctx GATs are pinned to the concrete
    // FFI ctx types here, so a `wire`/`init` body keeps its concrete ctx.
    let (boot_hooks, hotswap_hooks): (Vec<syn::ImplItemFn>, Vec<syn::ImplItemFn>) =
        lifecycle_methods
            .into_iter()
            .partition(|m| matches!(m.sig.ident.to_string().as_str(), "wire" | "unwire"));

    Ok(quote! {
        #actor_impl

        #(#handles_kind_impls)*

        impl #impl_generics ::aether_actor::Lifecycle for #self_ty #where_clause {
            #config_type_tokens
            type InitError = ::aether_actor::BootError;
            type InitCtx<'__a> = ::aether_actor::FfiInitCtx<'__a>;
            type Ctx<'__a> = ::aether_actor::FfiCtx<'__a>;

            #wrapped_init

            #(#boot_hooks)*
        }

        impl #impl_generics #trait_path for #self_ty #where_clause {
            #state_type_tokens

            #(#hotswap_hooks)*

            #generated_state_hooks
        }

        impl #impl_generics #self_ty #where_clause {
            #[doc(hidden)]
            pub fn __aether_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_actor::FfiCtx<'_, ::aether_actor::Manual>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                #dispatch_body
            }

            #inputs_manifest_consts

            #(#handler_methods_tokens)*
            #fallback_method_tokens
            #(#helper_methods_tokens)*
            #dehydrate_accessor_tokens
            #rehydrate_accessor_tokens
        }

        // ADR-0096: object-safe erasure so a multi-actor module's
        // `export!(A, B, …)` arm can hold whichever exported type an
        // instance became in one `Slot<Box<dyn ErasedFfiActor>>` and
        // route the FFI shims through it. Forwards to the inherent
        // dispatch table and the `FfiActor` lifecycle hooks; `init`
        // stays concrete (the `export!` arm tag-matches and boxes).
        impl #impl_generics ::aether_actor::ErasedFfiActor for #self_ty #where_clause {
            fn erased_namespace(&self) -> &'static str {
                <#self_ty as ::aether_actor::Actor>::NAMESPACE
            }
            fn erased_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_actor::FfiCtx<'_, ::aether_actor::Manual>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                self.__aether_dispatch(__aether_ctx, __aether_mail)
            }
            // ADR-0112: the lifecycle hooks keep their `FfiCtx<'_>` (= Single)
            // default signatures; downgrade the carried `Manual` ctx here.
            fn erased_wire(&mut self, __aether_ctx: &mut ::aether_actor::FfiCtx<'_, ::aether_actor::Manual>) {
                <#self_ty as ::aether_actor::Lifecycle>::wire(self, __aether_ctx.as_single());
            }
            fn erased_unwire(&mut self, __aether_ctx: &mut ::aether_actor::FfiCtx<'_, ::aether_actor::Manual>) {
                <#self_ty as ::aether_actor::Lifecycle>::unwire(self, __aether_ctx.as_single());
            }
            fn erased_on_dehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::FfiDropCtx<'_>,
            ) {
                <#self_ty as ::aether_actor::FfiActor>::on_dehydrate(self, __aether_ctx);
            }
            fn erased_on_rehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::FfiCtx<'_, ::aether_actor::Manual>,
                __aether_prior: ::aether_actor::PriorState<'_>,
            ) {
                <#self_ty as ::aether_actor::FfiActor>::on_rehydrate(self, __aether_ctx.as_single(), __aether_prior);
            }
        }

        #kind_retention_statics
    })
}

/// Issue 552 stage 1: expansion for `#[actor] impl NativeActor for X`
/// — the new native chassis-cap shape. Per-handler ctx + `&self`
/// (Arc-shared) + typed `init`. Mirrors `expand_wasm_actor`'s shape
/// across the wasm/native split.
///
/// Emits, all rooted in the consumer crate's namespace:
///   - `impl Actor for X` carrying the user-declared `const NAMESPACE`
///     (extracted from the impl block so the `NativeActor: Actor`
///     supertrait bound is satisfied).
///   - `impl HandlesKind<K> for X` per `#[handler]` method — the
///     compile-time gate `MailSender::send::<R, K>` consults.
///   - `impl NativeActor for X { type Config; fn init }` (the user's
///     bodies, attribute-stripped).
///   - `impl ::aether_substrate::NativeDispatch for X` whose body is
///     a kind-id if-chain that decodes payload via
///     `Kind::decode_from_bytes` and dispatches to the matching
///     handler method.
///   - The handler methods themselves (and any helper fns) on a
///     sibling inherent `impl X { … }`.
///
/// `#[fallback]` is rejected — native actors are typed receivers;
/// unknown kinds are programming errors, not fallback paths.
// Emits the full `NativeActor` surface in one walk: dispatch table,
// `init` wrapper, `HandlesKind<K>` impls per handler, plus the
// dispatch ABI plumbing. Splitting into helpers would force shared
// per-handler context structs without saving readability.
#[allow(clippy::too_many_lines)]
fn expand_native_actor_trait(item: ItemImpl, opts: ActorOpts) -> syn::Result<TokenStream2> {
    let self_ty = &item.self_ty;
    let generics = &item.generics;
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let trait_path = item
        .trait_
        .as_ref()
        .map(|(_, p, _)| p)
        .expect("trait_ checked above");

    let mut init_method: Option<syn::ImplItemFn> = None;
    let mut config_type: Option<syn::ImplItemType> = None;
    let mut handlers: Vec<NativeActorHandlerFn> = Vec::new();
    // ADR-0093 §3: `#[handler(task)]` completion handlers, collected
    // separately from mail handlers — they get no `HandlesKind<K>` impl
    // and aren't in the `aether.kinds.inputs` manifest (a completion is
    // not inbound mail), and they route by output type via a single
    // `TaskCompletionWake` dispatch arm rather than per-kind arms.
    let mut task_handlers: Vec<NativeActorTaskHandlerFn> = Vec::new();
    let mut fallback: Option<NativeFallbackFn> = None;
    let mut helpers: Vec<syn::ImplItemFn> = Vec::new();
    let mut consts: Vec<syn::ImplItemConst> = Vec::new();
    // Issue 584 (ADR-0079 amended): `wire` and `unwire` are
    // `NativeActor` trait methods with default empty bodies. When a
    // cap overrides them, the override must land inside the trait
    // impl block (so the dispatcher trampoline's `actor.wire(...)` /
    // `actor.unwire(...)` resolves to the override via trait
    // dispatch). Pre-issue-625 the macro routed every non-handler /
    // non-init fn into the inherent impl, so lifecycle overrides
    // triggered a dead_code warning and (worse) didn't override the
    // trait method at all.
    let mut lifecycle_methods: Vec<syn::ImplItemFn> = Vec::new();

    for impl_item in item.items {
        match impl_item {
            ImplItem::Type(it) if it.ident == "Config" => {
                config_type = Some(it);
            }
            ImplItem::Type(it) => {
                return Err(syn::Error::new_spanned(
                    it,
                    "#[actor] impl NativeActor for X accepts only `type Config = …` — \
                     other associated types aren't part of the trait",
                ));
            }
            ImplItem::Const(c) => {
                consts.push(c);
            }
            ImplItem::Fn(mut f) => {
                let handler_attr_idx = f.attrs.iter().position(attr_is_handler);
                let fallback_attr_idx = f.attrs.iter().position(attr_is_fallback);
                if handler_attr_idx.is_some() && fallback_attr_idx.is_some() {
                    return Err(syn::Error::new_spanned(
                        &f,
                        "method cannot be both #[handler] and #[fallback]",
                    ));
                }
                if let Some(idx) = handler_attr_idx {
                    let variant = parse_handler_variant(&f.attrs[idx])?;
                    // ADR-0112: read the reply class off the marker path
                    // (rejects `#[handler::stream]` on both the mail and task
                    // variants). A task handler always receives the
                    // downgraded `Single` ctx, so it carries no class field.
                    let class = parse_handler_class(&f.attrs[idx])?;
                    f.attrs.remove(idx);
                    match variant {
                        HandlerVariant::Mail => {
                            let (kind_ty, is_slice) = extract_native_actor_handler_kind(&f.sig)?;
                            let reply = classify_handler_reply(&f.sig.output);
                            handlers.push(NativeActorHandlerFn {
                                method: f,
                                kind_ty,
                                is_slice,
                                reply,
                                class,
                            });
                        }
                        HandlerVariant::Task => {
                            let (output_ty, context_ty, is_borrow) =
                                extract_task_handler_types(&f.sig)?;
                            let mode = classify_task_reply_mode(&f.sig, is_borrow)?;
                            task_handlers.push(NativeActorTaskHandlerFn {
                                method: f,
                                output_ty,
                                context_ty,
                                mode,
                            });
                        }
                    }
                } else if let Some(idx) = fallback_attr_idx {
                    if fallback.is_some() {
                        return Err(syn::Error::new_spanned(
                            &f,
                            "at most one #[fallback] method per native actor",
                        ));
                    }
                    validate_native_fallback_sig(&f.sig)?;
                    f.attrs.remove(idx);
                    fallback = Some(NativeFallbackFn { method: f });
                } else if f.sig.ident == "init" {
                    init_method = Some(f);
                } else if f.sig.ident == "wire" || f.sig.ident == "unwire" {
                    lifecycle_methods.push(f);
                } else {
                    helpers.push(f);
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unexpected item in #[actor] impl NativeActor for X (only fns, \
                     `type Config = …`, and `const` items are accepted)",
                ));
            }
        }
    }

    let init_method = init_method.ok_or_else(|| {
        syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor requires \
             `fn init(config: Self::Config, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError>`",
        )
    })?;

    let config_type = config_type.ok_or_else(|| {
        syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor requires `type Config = …` — \
             use `()` for caps without configuration",
        )
    })?;

    // Issue 576 + issue 603: native actors come in three flavours —
    // strict typed receiver (only `#[handler]`s), catch-all cap (only
    // `#[fallback]`), or hybrid (typed handlers + a `#[fallback]`
    // runtime safety net). `ComponentHostCapability` uses the hybrid
    // shape: declared `LoadComponent` / `DropComponent` / etc. land on
    // typed handlers; chassis-peripheral kinds (Phase 1 migration)
    // ride the fallback. The fallback runs only on dispatch table
    // misses, so per-handler `HandlesKind<K>` markers are still
    // authoritative at the type system — `ctx.actor::<X>().send(K)`
    // compiles only for declared K.
    if handlers.is_empty() && fallback.is_none() && task_handlers.is_empty() {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor requires at least one #[handler] method \
             or a #[fallback] method",
        ));
    }

    // ADR-0093 §3: two `#[handler(task)]` methods with the same
    // `TaskDone<O>` output type are ambiguous — completions route by `O`,
    // so a duplicate `O` would let the first-tried handler shadow the
    // second. Reject it at compile time, spanned at the later handler.
    for (i, later) in task_handlers.iter().enumerate() {
        if let Some(earlier) = task_handlers[..i]
            .iter()
            .find(|earlier| types_token_eq(&earlier.output_ty, &later.output_ty))
        {
            let earlier_name = &earlier.method.sig.ident;
            return Err(syn::Error::new_spanned(
                &later.method.sig.ident,
                format!(
                    "two #[handler(task)] methods share the `TaskDone<O>` output type \
                     (also on `{earlier_name}`) — completions route by output type, so a \
                     duplicate `O` is ambiguous (ADR-0093 §3). Give each task handler a \
                     distinct output type."
                ),
            ));
        }
    }

    // Two `#[handler]` methods that accept the same mail kind would emit
    // two `HandlesKind<K>` impls (a coherence error) plus a dead second
    // dispatch arm the first arm always shadows. Reject the duplicate at
    // compile time, spanned at the later handler. The macro has no type
    // resolution, so dedup is by token equality (`types_token_eq`,
    // matching the task-handler check above), not by resolved `KindId`.
    for (i, later) in handlers.iter().enumerate() {
        if let Some(earlier) = handlers[..i]
            .iter()
            .find(|earlier| types_token_eq(&earlier.kind_ty, &later.kind_ty))
        {
            let earlier_name = &earlier.method.sig.ident;
            let kind_ty = &later.kind_ty;
            return Err(syn::Error::new_spanned(
                &later.method.sig.ident,
                format!(
                    "two #[handler] methods accept the same mail kind `{}` (also on \
                     `{earlier_name}`) — each kind routes to exactly one handler. Give each \
                     handler a distinct kind.",
                    quote!(#kind_ty)
                ),
            ));
        }
    }

    // `NAMESPACE` is declared on the supertrait `Actor`, but the user
    // wrote it inside `impl NativeActor for X` for the symmetric
    // authoring shape. Route the const onto a sibling `impl Actor for X`
    // block so satisfying the supertrait bound works without making the
    // user split the impl.
    //
    // `skip_markers` (issue 565): when `#[bridge]` wraps a cfg-gated
    // `mod native` containing the actor block, it emits the always-on
    // `Actor` + `HandlesKind` impls itself as siblings of the mod and
    // rewrites this `#[actor]` to `#[actor(skip_markers)]` so this
    // expansion does not duplicate them. The native-only impls below
    // still emit unchanged.
    //
    //
    // Validate the const surface in one pass. Dispatch placement is no
    // longer authorable (issue 1187): the scheduling enum + trait const
    // were removed — every actor drains on the chassis worker pool, so a
    // leftover `SCHEDULING` const earns a pointed diagnostic. Any const
    // other than `NAMESPACE` is stray (the `Actor` super-trait carries no
    // other
    // authorable const) and is rejected at its own span rather than
    // silently routed onto the sibling `impl Actor` block; and the
    // presence of `NAMESPACE` is tracked so a block that omits it fails
    // here (spanned at the type, mirroring the `#[bridge]` path) instead
    // of at a later "no associated const NAMESPACE" error against the
    // surfaceless `Actor` trait. A `skip_markers` (bridge-wrapped) block
    // legitimately omits `NAMESPACE` — the `#[bridge]` expansion emits
    // the marker `impl Actor` carrying it as a sibling — so the
    // missing-NAMESPACE check is gated on `!opts.skip_markers`.
    let mut has_namespace = false;
    for c in &consts {
        if c.ident == "NAMESPACE" {
            has_namespace = true;
        } else if c.ident == "SCHEDULING" {
            return Err(syn::Error::new_spanned(
                c,
                "`SCHEDULING` was removed (issue 1187): every actor drains on the chassis \
                 worker pool. Drop the const — never block a handler; offload blocking work \
                 to a `ctx.spawn`'d thread that feeds results back as mail.",
            ));
        } else {
            return Err(syn::Error::new_spanned(
                c,
                "#[actor] impl NativeActor for X accepts only \
                 `const NAMESPACE: &'static str = …` — the `Actor` super-trait carries no \
                 other authorable const",
            ));
        }
    }
    if !has_namespace && !opts.skip_markers {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor for X must declare \
             `const NAMESPACE: &'static str = ...` so the marker `impl Actor` can carry it",
        ));
    }
    // NAMESPACE passes through unchanged because its RHS is a
    // primitive that doesn't require resolution.
    let const_tokens: Vec<TokenStream2> = consts.iter().map(|c| quote! { #c }).collect();
    let actor_impl = if opts.skip_markers || consts.is_empty() {
        quote! {}
    } else {
        quote! {
            impl #impl_generics ::aether_actor::Actor for #self_ty #where_clause {
                #(#const_tokens)*
            }
        }
    };

    // Issue 576 + issue 603: only-fallback (true catch-all) caps emit
    // a single blanket `impl<K: Kind> HandlesKind<K> for X {}` so any
    // typed `ctx.actor::<X>().send(&payload)` compiles for every K.
    // Strict receivers and hybrid caps (handlers + fallback safety
    // net) keep per-handler impls — only declared kinds compile via
    // typed sends; the fallback handles unknown kinds at runtime
    // (e.g. mail arriving by mailbox name).
    let handles_kind_impls: Vec<TokenStream2> = if opts.skip_markers {
        Vec::new()
    } else if fallback.is_some() && handlers.is_empty() {
        let kind_param: syn::Ident = syn::parse_quote!(__AetherCatchAllK);
        let mut blanket_generics = generics.clone();
        blanket_generics.params.push(syn::parse_quote!(
            #kind_param: ::aether_actor::__macro_internals::Kind
        ));
        let (blanket_impl, _, blanket_where) = blanket_generics.split_for_impl();
        vec![quote! {
            impl #blanket_impl ::aether_actor::HandlesKind<#kind_param>
                for #self_ty #blanket_where {}
        }]
    } else {
        handlers
            .iter()
            .map(|h| {
                let kind_ty = &h.kind_ty;
                quote! {
                    impl #impl_generics ::aether_actor::HandlesKind<#kind_ty>
                        for #self_ty #where_clause {}
                }
            })
            .collect()
    };

    // ADR-0081 retired the chassis-pushed `ConfigureLogDrain` mail;
    // see the matching note on the `#[actor]` derive path.

    let dispatch_arms = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        let method_ident = &h.method.sig.ident;
        // ADR-0112: the dispatch ctx is the full `Manual` view. A single
        // handler is called with the downgraded `as_single()` view and the
        // macro auto-replies a `-> R` return through `OutboundReply::reply`
        // on the `Manual` ctx (`-> ()` / `-> Pending<R>` discard it — the
        // deferred `Pending` send is #1805). A manual handler is called with
        // the `Manual` ctx directly and issues its own replies — no
        // auto-reply, regardless of return type.
        let call = match (h.class, &h.reply) {
            (HandlerClass::Single, HandlerReply::Sync(_)) => quote! {
                let __aether_reply = self.#method_ident(__aether_ctx.as_single(), __aether_decoded);
                ::aether_actor::OutboundReply::reply(__aether_ctx, &__aether_reply);
            },
            (HandlerClass::Single, HandlerReply::None | HandlerReply::Deferred(_)) => quote! {
                self.#method_ident(__aether_ctx.as_single(), __aether_decoded);
            },
            (HandlerClass::Manual, _) => quote! {
                self.#method_ident(__aether_ctx, __aether_decoded);
            },
            (HandlerClass::Stream, _) => {
                unreachable!("parse_handler_class rejects #[handler::stream]")
            }
        };
        if h.is_slice {
            // Slice handler — payload is `count * size_of::<K>()`
            // contiguous bytes (ADR-0019 batch wire). Cast to `&[K]`
            // for the handler. Only meaningful for cast-shape kinds;
            // postcard kinds have no batched wire shape.
            quote! {
                if __aether_kind.0 == <#kind_ty as ::aether_data::Kind>::ID.0 {
                    if let Some(__aether_decoded) =
                        ::aether_data::__derive_runtime::decode_cast_slice::<#kind_ty>(__aether_payload)
                    {
                        #call
                        return ::core::option::Option::Some(());
                    }
                    return ::core::option::Option::None;
                }
            }
        } else {
            quote! {
                if __aether_kind.0 == <#kind_ty as ::aether_data::Kind>::ID.0 {
                    if let Some(__aether_decoded) =
                        <#kind_ty as ::aether_data::Kind>::decode_from_bytes(__aether_payload)
                    {
                        #call
                        return ::core::option::Option::Some(());
                    }
                    return ::core::option::Option::None;
                }
            }
        }
    });

    // ADR-0093 §3: a SINGLE dispatch arm for all task completions. They
    // all arrive as `TaskCompletionWake` (carrying just a `DispatchId`);
    // the discriminant between task handlers is their `TaskDone<O, C>`
    // output type, not a kind id. Decode the id once, then try each task
    // handler's `(O, C)` via the non-consuming `try_take_task_done` — a
    // wrong-type probe leaves the ledger entry intact for a later
    // handler. `None` falls through to the default (unknown id / already
    // taken).
    let task_completion_arm = if task_handlers.is_empty() {
        quote! {}
    } else {
        let try_take_lines = task_handlers.iter().map(|t| {
            let output_ty = &t.output_ty;
            let context_ty = &t.context_ty;
            let method_ident = &t.method.sig.ident;
            // ADR-0109: how the completion discharges. By-value hands the
            // owned `TaskDone` to the handler (it self-resolves);
            // `&TaskDone -> R` calls the handler for the reply value then
            // `resolve_value`s it; `&TaskDone -> ()` releases the hold via
            // `release_no_reply` with no reply.
            //
            // ADR-0112: the dispatch ctx is the full `Manual` view; a task
            // handler (and `TaskDone::resolve_value`) take the single-mode
            // ctx, so downgrade with `as_single()`.
            let dispatch = match t.mode {
                TaskReplyMode::ByValue => quote! {
                    self.#method_ident(__aether_ctx.as_single(), __aether_done);
                },
                TaskReplyMode::BorrowReply => quote! {
                    let __aether_reply = self.#method_ident(__aether_ctx.as_single(), &__aether_done);
                    __aether_done.resolve_value(__aether_ctx.as_single(), &__aether_reply);
                },
                TaskReplyMode::BorrowNoReply => quote! {
                    self.#method_ident(__aether_ctx.as_single(), &__aether_done);
                    __aether_done.release_no_reply();
                },
            };
            quote! {
                if let ::core::option::Option::Some(__aether_done) =
                    __aether_ctx.try_take_task_done::<#output_ty, #context_ty>(__aether_dispatch_id)
                {
                    #dispatch
                    return ::core::option::Option::Some(());
                }
            }
        });
        quote! {
            if __aether_kind.0
                == <::aether_substrate::actor::native::TaskCompletionWake
                    as ::aether_data::Kind>::ID.0
            {
                let __aether_wake = match
                    <::aether_substrate::actor::native::TaskCompletionWake
                        as ::aether_data::Kind>::decode_from_bytes(__aether_payload)
                {
                    ::core::option::Option::Some(__aether_w) => __aether_w,
                    ::core::option::Option::None => return ::core::option::Option::None,
                };
                let __aether_dispatch_id =
                    ::aether_substrate::actor::native::DispatchId(__aether_wake.dispatch_id);
                #(#try_take_lines)*
                return ::core::option::Option::None;
            }
        }
    };

    let handler_methods: Vec<&syn::ImplItemFn> = handlers.iter().map(|h| &h.method).collect();
    let task_handler_methods: Vec<&syn::ImplItemFn> =
        task_handlers.iter().map(|t| &t.method).collect();
    let fallback_method = fallback.as_ref().map(|f| &f.method);
    let helper_methods = helpers.iter();

    // Issue 576: catch-all caps override `__aether_dispatch_fallback`
    // (the default-method on `NativeDispatch` returns `false`). The
    // strict-receiver path keeps the default. Catch-all caps also
    // emit an empty `__aether_dispatch_envelope` since there are no
    // typed handlers — the trampoline routes straight to the fallback
    // override on every envelope.
    let fallback_dispatch_override = fallback.as_ref().map(|f| {
        let method_ident = &f.method.sig.ident;
        // ADR-0112: the trait seam carries the `Manual` ctx; a `#[fallback]`
        // keeps its `NativeCtx<'_>` (= Single) signature, so downgrade.
        quote! {
            fn __aether_dispatch_fallback(
                &mut self,
                __aether_ctx: &mut ::aether_substrate::NativeCtx<'_, ::aether_actor::Manual>,
                __aether_env: &::aether_substrate::actor::native::envelope::Envelope,
            ) -> bool {
                self.#method_ident(__aether_ctx.as_single(), __aether_env);
                true
            }
        }
    });

    // iamacoffeepot/aether#1037: override `NativeDispatch::__aether_capabilities`
    // so native caps surface the same ADR-0033 receive-side capability
    // shape (handler kinds + `#[fallback]` presence) a wasm component
    // ships in its `aether.kinds.inputs` manifest. The native-cap-boot
    // path reads this to populate the queryable `CapabilityRegistry`,
    // unifying native + wasm dispatchability. Reply kinds are absent by
    // design — handlers promise nothing about replies. The handler
    // `doc` is dropped (the registry only needs ids + fallback flag),
    // so this is independent of rustdoc extraction.
    let capability_handler_entries = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        // ADR-0109 §5 / ADR-0112: native chassis caps don't yet surface a
        // per-handler reply contract — that needs a native handler
        // manifest (a follow-on). Report `ReplyContract::None` until then;
        // the wasm `describe_component` path carries the real class today.
        quote! {
            ::aether_substrate::actor::native::HandlerCapability {
                id: <#kind_ty as ::aether_data::Kind>::ID,
                name: <#kind_ty as ::aether_data::Kind>::NAME.to_owned(),
                doc: ::core::option::Option::None,
                reply: ::aether_data::ReplyContract::None,
            }
        }
    });
    let capability_fallback = if fallback.is_some() {
        quote! {
            ::core::option::Option::Some(
                ::aether_substrate::actor::native::FallbackCapability {
                    doc: ::core::option::Option::None,
                },
            )
        }
    } else {
        quote! { ::core::option::Option::None }
    };
    let capabilities_override = quote! {
        fn __aether_capabilities() -> ::aether_substrate::actor::native::ComponentCapabilities {
            ::aether_substrate::actor::native::ComponentCapabilities {
                handlers: ::std::vec![#(#capability_handler_entries),*],
                fallback: #capability_fallback,
                doc: ::core::option::Option::None,
                // ADR-0090 (issue 1257): native chassis caps don't carry
                // a describe-surfaced boot-config kind.
                config: ::core::option::Option::None,
            }
        }
    };

    // ADR-0109 §5: the native analogue of the wasm `aether.kinds.inputs`
    // custom section. Submit one link-time `HandlerEntry` per
    // `#[handler]` — the owning `NAMESPACE`, the input kind (id + name),
    // and the reply kind id read off the return type (the same
    // `classify_handler_reply` the auto-reply arm uses). The
    // `aether.inventory` cap folds these into the
    // `aether.inventory.handlers` reply, so a native cap surfaces its
    // `In -> Out` the way `describe_component` does for a wasm component.
    // Gated on `not(wasm32)` to match the rest of the native surface
    // (the `inventory` crate doesn't link on wasm32). Skipped for a
    // generic native actor — none exist, and `<Self as Actor>::NAMESPACE`
    // wouldn't const-resolve in the non-generic inventory static.
    let handler_inventory = if generics.params.is_empty() {
        let submissions = handlers.iter().map(|h| {
            let kind_ty = &h.kind_ty;
            let reply_expr = if let Some(reply_ty) = h.reply.manifest_kind() {
                quote! { ::core::option::Option::Some(<#reply_ty as ::aether_data::Kind>::ID) }
            } else {
                quote! { ::core::option::Option::None }
            };
            quote! {
                #[cfg(not(target_arch = "wasm32"))]
                ::aether_data::name_inventory::inventory::submit! {
                    ::aether_data::name_inventory::HandlerEntry {
                        namespace: <#self_ty as ::aether_actor::Actor>::NAMESPACE,
                        id: <#kind_ty as ::aether_data::Kind>::ID,
                        name: <#kind_ty as ::aether_data::Kind>::NAME,
                        reply: #reply_expr,
                    }
                }
            }
        });
        quote! { #(#submissions)* }
    } else {
        quote! {}
    };

    // Issue 552 stage 4: NativeActor + NativeDispatch + the inherent
    // handler-method impl all reach for `::aether_substrate::*` paths
    // and native-only types in their bodies. They're emitted under
    // `#[cfg(not(target_arch = "wasm32"))]` so `aether-capabilities`
    // can compile for `wasm32-unknown-unknown` without the substrate
    // dep — wasm consumers see only the always-on Actor +
    // HandlesKind markers, which is enough for typed
    // `ctx.actor::<R>().send(...)` against cap markers.
    //
    // Gate is `target_arch` not `feature = "native"` because
    // NativeActor/NativeDispatch are wasm-incompatible by definition;
    // there's no realistic case where a host build wants to skip
    // them. Pinning the cfg in the macro means consumer crates never
    // have to define matching feature flags.
    Ok(quote! {
        #actor_impl

        #(#handles_kind_impls)*

        #handler_inventory

        // iamacoffeepot/aether#2048: the boot lifecycle (`init` / `wire` /
        // `unwire` + `type Config`) lives on the shared `Lifecycle`
        // capability, with the per-target ctx GATs pinned to the concrete
        // native ctx types so an `init`/`wire` body keeps its concrete ctx.
        #[cfg(not(target_arch = "wasm32"))]
        impl #impl_generics ::aether_actor::Lifecycle for #self_ty #where_clause {
            #config_type
            type InitError = ::aether_substrate::BootError;
            type InitCtx<'__a> = ::aether_substrate::NativeInitCtx<'__a>;
            type Ctx<'__a> = ::aether_substrate::NativeCtx<'__a>;
            #init_method
            #(#lifecycle_methods)*
        }

        // `NativeActor` is now the empty composition `Actor +
        // Lifecycle<InitError = BootError>`; per-kind dispatch lives on the
        // sibling `NativeDispatch` impl below.
        #[cfg(not(target_arch = "wasm32"))]
        impl #impl_generics #trait_path for #self_ty #where_clause {}

        #[cfg(not(target_arch = "wasm32"))]
        impl #impl_generics ::aether_substrate::NativeDispatch for #self_ty #where_clause {
            // ADR-0112: the dispatch seam carries the most-permissive
            // `Manual` ctx; the arms downgrade per handler class.
            fn __aether_dispatch_envelope(
                &mut self,
                __aether_ctx: &mut ::aether_substrate::NativeCtx<'_, ::aether_actor::Manual>,
                __aether_kind: ::aether_substrate::mail::KindId,
                __aether_payload: &[u8],
            ) -> ::core::option::Option<()> {
                #(#dispatch_arms)*
                #task_completion_arm
                ::core::option::Option::None
            }

            #fallback_dispatch_override

            #capabilities_override
        }

        #[cfg(not(target_arch = "wasm32"))]
        impl #impl_generics #self_ty #where_clause {
            #(#handler_methods)*
            #(#task_handler_methods)*
            #fallback_method
            #(#helper_methods)*
        }
    })
}

struct NativeActorHandlerFn {
    method: syn::ImplItemFn,
    kind_ty: Type,
    /// `true` when the handler's `mail` parameter is `&[K]` rather
    /// than `K`. The dispatcher decodes via `decode_cast_slice` so a
    /// single envelope with `count > 1` reaches the handler intact.
    is_slice: bool,
    /// ADR-0109: the handler's reply contract, classified from its
    /// return type. A `-> R` native handler auto-replies `R` through
    /// `OutboundReply::reply`, the same path a manual `ctx.reply` takes.
    reply: HandlerReply,
    /// ADR-0112: the declared reply class (single / manual). Selects the
    /// ctx view the dispatch arm passes and the manifest reply tag.
    class: HandlerClass,
}

/// A `#[handler(task)]` completion handler (ADR-0093 §3). Its third
/// parameter is `done: TaskDone<O, C>` (C defaults to `()`); `output_ty`
/// / `context_ty` are the extracted `O` / `C`. Routed not by a kind id
/// but by output type, via a non-consuming `try_take_task_done::<O, C>`
/// probe in the single `TaskCompletionWake` dispatch arm.
struct NativeActorTaskHandlerFn {
    method: syn::ImplItemFn,
    output_ty: Type,
    context_ty: Type,
    /// ADR-0109: how the completion discharges its reply — self-resolve
    /// (by-value), macro-driven `resolve_value` (`&TaskDone -> R`), or
    /// `release_no_reply` (`&TaskDone -> ()`).
    mode: TaskReplyMode,
}

/// Token-level type equality, used to reject duplicate `TaskDone<O>`
/// output types across `#[handler(task)]` methods. `syn::Type` is not
/// `PartialEq`, so compare the pretty-printed token streams — exact
/// enough for the duplicate-`O` ambiguity check (two spellings of the
/// same type that tokenize differently are a corner case the author can
/// resolve by normalising).
fn types_token_eq(a: &Type, b: &Type) -> bool {
    quote!(#a).to_string() == quote!(#b).to_string()
}

/// Issue 576: native-side `#[fallback]` collected on a
/// `#[actor] impl NativeActor for X` block. Mirrors the wasm-side
/// [`FallbackFn`] but the native handler signature pivots on
/// [`Envelope`] — it carries the kind id, kind name, origin, sender,
/// and payload in one borrow so catch-all caps (broadcast, future
/// hub-as-actor) can lift the whole envelope into a downstream call
/// without rebuilding fields the trampoline already has.
///
/// [`Envelope`]: aether_substrate::actor::native::envelope::Envelope
struct NativeFallbackFn {
    method: syn::ImplItemFn,
}

/// Validate a native `#[fallback]` method signature. Required shape:
/// `(&self | &mut self, ctx: &mut NativeCtx<'_>, env: &Envelope)`.
/// The third argument's exact type isn't checked here — the
/// synthesized override calls `self.<fallback>(ctx, env)` and the
/// user's fn body will type-error against `&Envelope` if they wrote
/// the wrong parameter type.
///
/// Issue 629 / Phase B: `&mut self` is now allowed alongside `&self`.
/// The dispatcher owns the cap as `Box<A>` and calls the fallback
/// through `&mut Box<A>`, so either receiver shape works.
fn validate_native_fallback_sig(sig: &Signature) -> syn::Result<()> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[fallback] on `impl NativeActor for X` must have signature \
             `(&self | &mut self, ctx: &mut NativeCtx<'_>, env: &Envelope)`",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(
            first,
            "#[fallback] first parameter must be `&self` or `&mut self`",
        ));
    }
    let third = &sig.inputs[2];
    if !matches!(third, FnArg::Typed(_)) {
        return Err(syn::Error::new_spanned(
            third,
            "#[fallback] third parameter must be `env: &Envelope`",
        ));
    }
    Ok(())
}

/// Extract `K` from a `#[actor] impl NativeActor` handler method's
/// third parameter and a flag for slice-handler shape. Accepts:
///   - `(&self | &mut self, ctx: &mut NativeCtx<'_>, mail: K)` —
///     single-payload handler, decodes via `Kind::decode_from_bytes`.
///   - `(&self | &mut self, ctx: &mut NativeCtx<'_>, mails: &[K])` —
///     batched cast-shape handler, decodes the whole envelope as a
///     contiguous `&[K]` slice via `decode_cast_slice` so a single
///     envelope with `count > 1` (`Mailbox::send_many`, ADR-0019)
///     reaches the handler intact. Only meaningful for cast-shape
///     kinds; postcard kinds have no batch wire.
///
/// Issue 629 / Phase B: `&mut self` is now allowed alongside `&self`.
/// The dispatcher owns the cap as `Box<A>` and calls each handler
/// through `&mut Box<A>`, so either receiver shape works. Caps with
/// mutable state migrate from interior mutability (`Mutex` / `Atomic`)
/// to plain fields by flipping handler signatures to `&mut self` per
/// cap.
fn extract_native_actor_handler_kind(sig: &Signature) -> syn::Result<(Type, bool)> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[actor] impl NativeActor #[handler] method must have signature \
             `(&self | &mut self, ctx: &mut NativeCtx<'_>, arg: K)` \
             (or `mail: &[K]` for batched cast kinds)",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(
            first,
            "#[handler] first parameter must be `&self` or `&mut self`",
        ));
    }
    let third = &sig.inputs[2];
    let FnArg::Typed(pt) = third else {
        return Err(syn::Error::new_spanned(
            third,
            "#[handler] third parameter must be a typed `arg: K` or `mail: &[K]`",
        ));
    };
    // Detect `&[K]` slice handlers (any reference to a slice). Inner
    // `K` is what `HandlesKind` / `Kind::ID` reference.
    if let Type::Reference(type_ref) = &*pt.ty
        && let Type::Slice(slice) = &*type_ref.elem
    {
        return Ok(((*slice.elem).clone(), true));
    }
    Ok(((*pt.ty).clone(), false))
}

/// Extract `K` from a handler method's third parameter (`arg: K`).
/// Accepts any type path — trait-bound validation lives in the
/// generated call site: the `mail.decode_typed::<K>()` in the
/// synthesized dispatcher requires `K: Kind + AnyBitPattern + 'static`,
/// so unsupported types surface as a trait-bound error pointing at
/// the user's signature.
fn extract_handler_kind_type(sig: &Signature) -> syn::Result<Type> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[handler] method must have signature `(&mut self, ctx: &mut Ctx<'_>, arg: K)`",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(
            first,
            "#[handler] first parameter must be `&mut self`",
        ));
    }
    let third = &sig.inputs[2];
    let FnArg::Typed(pt) = third else {
        return Err(syn::Error::new_spanned(
            third,
            "#[handler] third parameter must be a typed `arg: K`",
        ));
    };
    Ok((*pt.ty).clone())
}

/// ADR-0109: a `#[handler]`'s reply contract, read off its return type.
/// The return type is the single source of truth for what a handler
/// replies — there is no separate `#[handler(reply = X)]` annotation.
enum HandlerReply {
    /// `-> ()` or no return type — fire-and-forget, replies nothing.
    None,
    /// `-> R: Kind` — reply `R` to the inbound sender synchronously on
    /// handler return, routed through the inbound guard's reply path.
    Sync(Type),
    /// `-> Pending<R>` — `R` is the deferred reply kind, discharged
    /// later via ADR-0093's hold ledger. The classifier recognizes the
    /// shape and publishes `R` to the manifest; the deferred send
    /// itself is wired in a follow-on (iamacoffeepot/aether#1805), so no
    /// synchronous reply is emitted for this arm here.
    Deferred(Type),
}

impl HandlerReply {
    /// The reply kind published to the `aether.kinds.inputs` manifest:
    /// `R` for both the synchronous and deferred arms (ADR-0109 §4 reads
    /// the inner `R` off `-> Pending<R>`), `None` for `-> ()`.
    fn manifest_kind(&self) -> Option<&Type> {
        match self {
            Self::None => None,
            Self::Sync(ty) | Self::Deferred(ty) => Some(ty),
        }
    }
}

/// Classify a handler's return type into a [`HandlerReply`] (ADR-0109).
/// `-> ()` (or an omitted return) is fire-and-forget; `-> Pending<R>`
/// — a path whose last segment is `Pending<R>` — is the deferred arm;
/// anything else is a synchronous `-> R` reply. The classifier is
/// purely syntactic (the macro has no type resolution): `R`'s `Kind`
/// bound is checked at the generated `ctx.reply(&r)` call site / the
/// `<R as Kind>::ID` manifest term.
fn classify_handler_reply(output: &ReturnType) -> HandlerReply {
    let ty = match output {
        ReturnType::Default => return HandlerReply::None,
        ReturnType::Type(_, ty) => ty.as_ref(),
    };
    // `-> ()` — the empty tuple — replies nothing, same as no return.
    if let Type::Tuple(tuple) = ty
        && tuple.elems.is_empty()
    {
        return HandlerReply::None;
    }
    // `-> Pending<R>` — last path segment `Pending` with one type arg.
    if let Type::Path(type_path) = ty
        && let Some(seg) = type_path.path.segments.last()
        && seg.ident == "Pending"
        && let PathArguments::AngleBracketed(args) = &seg.arguments
        && let Some(GenericArgument::Type(inner)) = args.args.first()
    {
        return HandlerReply::Deferred(inner.clone());
    }
    HandlerReply::Sync(ty.clone())
}

/// Soft validation that a `#[fallback]` method's signature is shaped
/// for `Mail<'_>`. We don't do deep type equality against
/// `::aether_actor::Mail<'_>` — the synthesized dispatcher's call
/// to `self.<fallback>(ctx, mail)` will type-check at the call site
/// and produce a clear error if the user wrote the wrong arg type.
fn validate_fallback_sig(sig: &Signature) -> syn::Result<()> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[fallback] method must have signature `(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>)`",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(
            first,
            "#[fallback] first parameter must be `&mut self`",
        ));
    }
    let third = &sig.inputs[2];
    if !matches!(third, FnArg::Typed(_)) {
        return Err(syn::Error::new_spanned(
            third,
            "#[fallback] third parameter must be `mail: Mail<'_>`",
        ));
    }
    Ok(())
}

/// Synthesized body of `__aether_dispatch`. Compares `mail.kind()`
/// against each `<K as Kind>::ID` const (ADR-0030 Phase 2); on match,
/// decodes via `Mail::decode_kind::<K>()` and calls the inherent-impl
/// handler method. Wire shape (cast vs postcard) is picked at K's
/// `Kind` derive site, not here — `Kind::decode_from_bytes` carries
/// the per-K body — so this dispatcher never sees the wire choice.
/// Returns `DISPATCH_HANDLED` on a match or when the `#[fallback]`
/// ran; returns `DISPATCH_UNKNOWN_KIND` on strict-receiver miss so
/// the substrate's scheduler logs the drop (issue #142).
fn build_dispatch_body(handlers: &[HandlerFn], fallback: Option<&FallbackFn>) -> TokenStream2 {
    let arms = handlers.iter().map(|h| {
        let k = &h.kind_ty;
        let method = &h.method.sig.ident;
        // ADR-0112: the dispatch ctx is the full `Manual` view. A single
        // handler is called with the downgraded `as_single()` view and the
        // macro auto-replies a `-> R` return through `OutboundReply::reply`
        // on the `Manual` ctx (`-> ()` / `-> Pending<R>` discard it — the
        // deferred `Pending` send is #1805). A manual handler is called with
        // the `Manual` ctx directly and issues its own replies — no
        // auto-reply, regardless of return type.
        let call = match (h.class, &h.reply) {
            (HandlerClass::Single, HandlerReply::Sync(_)) => quote! {
                let __aether_reply = self.#method(__aether_ctx.as_single(), __aether_decoded);
                ::aether_actor::OutboundReply::reply(__aether_ctx, &__aether_reply);
            },
            (HandlerClass::Single, HandlerReply::None | HandlerReply::Deferred(_)) => quote! {
                self.#method(__aether_ctx.as_single(), __aether_decoded);
            },
            (HandlerClass::Manual, _) => quote! {
                self.#method(__aether_ctx, __aether_decoded);
            },
            (HandlerClass::Stream, _) => {
                unreachable!("parse_handler_class rejects #[handler::stream]")
            }
        };
        // `Mail::kind()` returns the raw `u64` the FFI carried; `Kind::ID`
        // is typed `KindId` post-issue 466, so we drop into `.0` for the
        // comparison.
        quote! {
            if __aether_kind == <#k as ::aether_actor::__macro_internals::Kind>::ID.0 {
                if let ::core::option::Option::Some(__aether_decoded) =
                    __aether_mail.decode_kind::<#k>()
                {
                    #call
                }
                return ::aether_actor::DISPATCH_HANDLED;
            }
        }
    });

    let tail = if let Some(f) = fallback {
        let method = &f.method.sig.ident;
        // ADR-0112: a `#[fallback]` keeps its `FfiCtx<'_>` (= Single)
        // signature; the dispatch ctx is `Manual`, so downgrade.
        quote! {
            self.#method(__aether_ctx.as_single(), __aether_mail);
            ::aether_actor::DISPATCH_HANDLED
        }
    } else {
        quote! { ::aether_actor::DISPATCH_UNKNOWN_KIND }
    };

    // ADR-0081 retired the chassis-pushed `ConfigureLogDrain` mail —
    // each actor's `ActorLogRing` lives in its own `ActorSlots`, so
    // there is no drain target to wire. The auto-emitted dispatch arm
    // that consumed that mail retired alongside it.

    quote! {
        let __aether_kind = __aether_mail.kind();
        __aether_ctx.__set_reply_to(__aether_mail.reply_handle());
        #( #arms )*
        #tail
    }
}

/// Emit two associated consts inside the component's inherent impl —
/// `__AETHER_INPUTS_MANIFEST_LEN: usize` and
/// `__AETHER_INPUTS_MANIFEST: [u8; …LEN]` — carrying the
/// concatenated `aether.kinds.inputs` record bytes. Each record is
/// `[INPUTS_SECTION_VERSION (0x05), ..wire(InputsRecord)..]`,
/// assembled at const-eval via the hub-protocol const-fn encoders.
/// `aether_actor::export!()` reads these consts and emits the
/// `#[unsafe(link_section = "aether.kinds.inputs")]` static in the
/// cdylib root crate. Keeping the section emission out of this macro
/// is what prevents the section from stacking when a `#[actor]`-
/// using crate is pulled in as a wasm32 rlib by another cdylib (a
/// rlib that doesn't call `export!()` contributes no section bytes).
// One const-eval byte-copy block per record kind (handler / fallback /
// component-doc / config); inlining them in one walk keeps each emitted
// block contiguous and reads better than four near-identical helpers.
#[allow(clippy::too_many_lines)]
fn build_inputs_manifest_consts(
    handlers: &[HandlerFn],
    fallback: Option<&FallbackFn>,
    component_doc: Option<&String>,
    config_kind_ty: Option<&Type>,
) -> TokenStream2 {
    let mut len_terms: Vec<TokenStream2> = Vec::new();
    let mut copy_blocks: Vec<TokenStream2> = Vec::new();

    for h in handlers {
        let k = &h.kind_ty;
        let doc_expr = option_str_token(h.agent_doc.as_ref());
        // ADR-0112: the reply class rides the handler record as a
        // `ReplyContract` `(tag, id)` pair — `(0, 0)` for a single `-> ()`,
        // `(1, R::ID)` for a single `-> R` / `-> Pending<R>`, `(3, 0)` for a
        // manual handler (no single static reply kind). `Stream` is
        // unreachable — `parse_handler_class` rejects `#[handler::stream]`.
        let (reply_tag_expr, reply_id_expr) = match (h.class, h.reply.manifest_kind()) {
            (HandlerClass::Manual, _) => (quote! { 3u8 }, quote! { 0u64 }),
            (HandlerClass::Single, Some(r)) => (
                quote! { 1u8 },
                quote! { <#r as ::aether_actor::__macro_internals::Kind>::ID.0 },
            ),
            (HandlerClass::Single, None) => (quote! { 0u8 }, quote! { 0u64 }),
            (HandlerClass::Stream, _) => {
                unreachable!("parse_handler_class rejects #[handler::stream]")
            }
        };
        // `inputs_handler_len` / `write_inputs_handler` take a raw `u64`
        // for the wire bytes; `Kind::ID` is `KindId` post-issue 466 so
        // we drop into `.0` here.
        len_terms.push(quote! {
            (1 + ::aether_actor::__macro_internals::canonical::inputs_handler_len(
                <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                #doc_expr,
                #reply_tag_expr,
                #reply_id_expr,
            ))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_actor::__macro_internals::canonical::inputs_handler_len(
                        <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                        <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                        #doc_expr,
                        #reply_tag_expr,
                        #reply_id_expr,
                    );
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_actor::__macro_internals::canonical::write_inputs_handler::<REC_LEN>(
                        <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                        <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                        #doc_expr,
                        #reply_tag_expr,
                        #reply_id_expr,
                    );
                // Per-record section version byte, bumped to 0x05 by
                // ADR-0118 (issue 1984) — the record is now the owned
                // aether-wire encoding rather than postcard. Keep in
                // lockstep with `INPUTS_SECTION_VERSION`.
                out[pos] = 0x05;
                pos += 1;
                let mut i = 0;
                while i < REC_LEN {
                    out[pos] = REC_BYTES[i];
                    pos += 1;
                    i += 1;
                }
            }
        });
    }

    if let Some(f) = fallback {
        let doc_expr = option_str_token(f.agent_doc.as_ref());
        len_terms.push(quote! {
            (1 + ::aether_actor::__macro_internals::canonical::inputs_fallback_len(#doc_expr))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_actor::__macro_internals::canonical::inputs_fallback_len(#doc_expr);
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_actor::__macro_internals::canonical::write_inputs_fallback::<REC_LEN>(#doc_expr);
                // Per-record section version byte, in lockstep with
                // `INPUTS_SECTION_VERSION` (0x05, ADR-0118 / issue 1984).
                out[pos] = 0x05;
                pos += 1;
                let mut i = 0;
                while i < REC_LEN {
                    out[pos] = REC_BYTES[i];
                    pos += 1;
                    i += 1;
                }
            }
        });
    }

    if let Some(doc) = component_doc {
        let doc_lit = doc.as_str();
        len_terms.push(quote! {
            (1 + ::aether_actor::__macro_internals::canonical::inputs_component_len(#doc_lit))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_actor::__macro_internals::canonical::inputs_component_len(#doc_lit);
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_actor::__macro_internals::canonical::write_inputs_component::<REC_LEN>(#doc_lit);
                // Per-record section version byte, in lockstep with
                // `INPUTS_SECTION_VERSION` (0x05, ADR-0118 / issue 1984).
                out[pos] = 0x05;
                pos += 1;
                let mut i = 0;
                while i < REC_LEN {
                    out[pos] = REC_BYTES[i];
                    pos += 1;
                    i += 1;
                }
            }
        });
    }

    // ADR-0090 (issue 1257): emit a `Config` record keyed by the
    // declared config kind's `Kind::ID` / `NAME`. Only present when the
    // user spelled `type Config = …` — `config_kind_ty` is `None` for
    // the macro-synthesized `()` case, so a no-config component stays
    // clean. Variant tag `0x03` matches `InputsRecord::Config`.
    if let Some(cfg) = config_kind_ty {
        len_terms.push(quote! {
            (1 + ::aether_actor::__macro_internals::canonical::inputs_config_len(
                <#cfg as ::aether_actor::__macro_internals::Kind>::ID.0,
                <#cfg as ::aether_actor::__macro_internals::Kind>::NAME,
            ))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_actor::__macro_internals::canonical::inputs_config_len(
                        <#cfg as ::aether_actor::__macro_internals::Kind>::ID.0,
                        <#cfg as ::aether_actor::__macro_internals::Kind>::NAME,
                    );
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_actor::__macro_internals::canonical::write_inputs_config::<REC_LEN>(
                        <#cfg as ::aether_actor::__macro_internals::Kind>::ID.0,
                        <#cfg as ::aether_actor::__macro_internals::Kind>::NAME,
                    );
                // Per-record section version byte (0x05), in lockstep
                // with `INPUTS_SECTION_VERSION` (ADR-0118 / issue 1984).
                out[pos] = 0x05;
                pos += 1;
                let mut i = 0;
                while i < REC_LEN {
                    out[pos] = REC_BYTES[i];
                    pos += 1;
                    i += 1;
                }
            }
        });
    }

    let len_expr = if len_terms.is_empty() {
        // Unreachable in practice — `handlers_impl` rejects the empty
        // case earlier — but keep the const arithmetic well-typed so
        // a stripped-down macro test with no records still compiles.
        quote! { 0usize }
    } else {
        quote! { #(#len_terms)+* }
    };

    quote! {
        #[doc(hidden)]
        pub const __AETHER_INPUTS_MANIFEST_LEN: usize = #len_expr;

        #[doc(hidden)]
        pub const __AETHER_INPUTS_MANIFEST: [u8; Self::__AETHER_INPUTS_MANIFEST_LEN] = {
            let mut out = [0u8; Self::__AETHER_INPUTS_MANIFEST_LEN];
            let mut pos: usize = 0usize;
            #(#copy_blocks)*
            let _ = pos;
            out
        };
    }
}

/// Emit `#[link_section = "aether.kinds"]` statics in the consumer
/// crate — one per `#[handler]`-handled kind — so every kind the
/// component listens for survives wasm-ld dead-section stripping.
///
/// Why this exists: the `Kind` derive emits `aether.kinds` and
/// `aether.kinds.labels` statics in the *defining* crate. When the
/// kind lives in a dependency rlib (e.g. `aether-kinds` or a shared
/// demo crate), the linker strips those statics from the final cdylib
/// because no Rust code in the consumer references the symbol by
/// name. `#[used]` keeps the symbol in the rlib's object file but
/// doesn't cross the rlib→cdylib boundary under `--gc-sections`. The
/// `aether.kinds.inputs` section survives only because
/// `#[actor]` emits it here, in the consumer's own compilation
/// unit. We apply the same trick to `aether.kinds`.
///
/// The bytes are computed via trait dispatch on `<K as Kind>::NAME`
/// and `<K as Schema>::SCHEMA` so this doesn't require the kind's
/// derive to expose its private canonical-bytes statics. Duplicate
/// records (one from the defining crate when it also builds as a
/// cdylib, one here in the consumer) are harmless: the substrate's
/// `register_kind_with_descriptor` is idempotent on `(name, schema)`
/// match (ADR-0030 Phase 2).
///
/// Scope is limited to handler-side kinds — kinds the component only
/// *sends* don't need local retention because the receiving substrate
/// is responsible for having them registered (it either hosts a
/// component that declares them, or is the hub with its own server
/// component). If that assumption ever breaks, extend this emitter to
/// walk `Sink<K>` resolutions too.
#[allow(clippy::too_many_lines)] // per-handler retention static block; one walk keeps each emitted static contiguous
fn build_kinds_section_retention_statics(
    self_ty: &Type,
    handlers: &[HandlerFn],
    config_kind_ty: Option<&Type>,
) -> TokenStream2 {
    let self_ty_hint = type_hint(self_ty);

    // ADR-0090 (issue 1257): the declared config kind needs the same
    // `aether.kinds` / `aether.kinds.labels` retention as handler kinds
    // so its schema + labels survive the rlib→cdylib dead-section strip
    // and `describe_kinds` can resolve it by id. Tack it onto the walk
    // with a distinct index suffix so it never collides with a handler
    // static. `None` (synthesized `()` config) contributes nothing.
    // The suffix is a plain `String` (e.g. "0", "1", "CONFIG") spliced
    // into the larger static identifiers below — a bare numeric ident
    // isn't valid on its own, so it must stay a format arg, not a
    // standalone `Ident`.
    let retained_kinds: Vec<(Type, String)> = handlers
        .iter()
        .enumerate()
        .map(|(idx, h)| (h.kind_ty.clone(), idx.to_string()))
        .chain(config_kind_ty.map(|cfg| (cfg.clone(), "CONFIG".to_string())))
        .collect();

    let statics = retained_kinds.iter().map(|(k, idx)| {
        let schema_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_SCHEMA_{}_{}",
            self_ty_hint,
            idx
        );
        let len_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_CANONICAL_LEN_{}_{}",
            self_ty_hint,
            idx
        );
        let bytes_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_CANONICAL_BYTES_{}_{}",
            self_ty_hint,
            idx
        );
        let section_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_MANIFEST_{}_{}",
            self_ty_hint,
            idx
        );
        let labels_static_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_LABELS_{}_{}",
            self_ty_hint,
            idx
        );
        let labels_len_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_LABELS_LEN_{}_{}",
            self_ty_hint,
            idx
        );
        let labels_bytes_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_LABELS_BYTES_{}_{}",
            self_ty_hint,
            idx
        );
        let labels_section_ident = quote::format_ident!(
            "__AETHER_HANDLERS_KIND_LABELS_MANIFEST_{}_{}",
            self_ty_hint,
            idx
        );
        quote! {
            // Mirrors the intermediate-static pattern in `expand_kind`
            // (mail-derive/src/lib.rs) so const-eval of the serializer
            // sees a `&'static SchemaType` / `&'static KindLabels`
            // instead of materializing a temporary whose non-trivial
            // Drop can't run at compile time.
            static #schema_ident: ::aether_actor::__macro_internals::SchemaType =
                <#k as ::aether_actor::__macro_internals::Schema>::SCHEMA;
            const #len_ident: usize =
                ::aether_actor::__macro_internals::canonical::canonical_len_kind(
                    <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                    &#schema_ident,
                );
            const #bytes_ident: [u8; #len_ident] =
                ::aether_actor::__macro_internals::canonical::canonical_serialize_kind::<#len_ident>(
                    <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                    &#schema_ident,
                );
            // Same v0x05 wire shape as `expand_kind`'s primary emission
            // (ADR-0118 / issue 1984: the owned aether-wire encoding) so
            // retention records (when this kind lives in a dependency
            // rlib) pair cleanly with the primary records by id.
            #[cfg(target_arch = "wasm32")]
            #[used]
            #[unsafe(link_section = "aether.kinds")]
            static #section_ident: [u8; #len_ident + 1] = {
                let mut out = [0u8; #len_ident + 1];
                out[0] = 0x05;
                let mut i = 0;
                while i < #len_ident {
                    out[i + 1] = #bytes_ident[i];
                    i += 1;
                }
                out
            };

            // Parallel labels retention. Without this, kinds defined
            // in a dependency rlib (whose Kind-derive labels get
            // stripped at rlib→cdylib) survive via `aether.kinds`
            // retention but have no labels counterpart, and the
            // reader can't reconstruct named fields — the symptom the
            // by-id pairing replaced by-index pairing to avoid.
            // `kind_label` falls back to the empty string for types
            // without a `Schema::LABEL` (none today — every derived
            // Kind sets one — but defensive against future hand-rolled
            // Schema impls).
            static #labels_static_ident: ::aether_actor::__macro_internals::KindLabels =
                ::aether_actor::__macro_internals::KindLabels {
                    // Issue 469: `KindLabels.kind_id` is typed
                    // `KindId` end-to-end; pass through directly.
                    kind_id: <#k as ::aether_actor::__macro_internals::Kind>::ID,
                    kind_label: ::aether_actor::__macro_internals::Cow::Borrowed(
                        match <#k as ::aether_actor::__macro_internals::Schema>::LABEL {
                            ::core::option::Option::Some(s) => s,
                            ::core::option::Option::None => "",
                        },
                    ),
                    root: <#k as ::aether_actor::__macro_internals::Schema>::LABEL_NODE,
                };
            const #labels_len_ident: usize =
                ::aether_actor::__macro_internals::canonical::canonical_len_labels(
                    &#labels_static_ident,
                );
            const #labels_bytes_ident: [u8; #labels_len_ident] =
                ::aether_actor::__macro_internals::canonical::canonical_serialize_labels::<#labels_len_ident>(
                    &#labels_static_ident,
                );
            #[cfg(target_arch = "wasm32")]
            #[used]
            #[unsafe(link_section = "aether.kinds.labels")]
            static #labels_section_ident: [u8; #labels_len_ident + 1] = {
                let mut out = [0u8; #labels_len_ident + 1];
                // v0x04 (ADR-0118 / issue 1984): the owned aether-wire
                // encoding of `KindLabels`, matching `expand_kind`.
                out[0] = 0x04;
                let mut i = 0;
                while i < #labels_len_ident {
                    out[i + 1] = #labels_bytes_ident[i];
                    i += 1;
                }
                out
            };
        }
    });

    quote! { #(#statics)* }
}

/// Produce an identifier-safe hint from the Self type. For a plain
/// type path (`InputLogger`, `my_crate::Hello`), use the last segment;
/// otherwise fall back to "COMPONENT" so the statics still compile.
fn type_hint(ty: &Type) -> syn::Ident {
    if let Type::Path(tp) = ty
        && let Some(seg) = tp.path.segments.last()
    {
        return syn::Ident::new(
            &to_screaming_snake_case(&seg.ident.to_string()),
            seg.ident.span(),
        );
    }
    syn::Ident::new("COMPONENT", proc_macro2::Span::call_site())
}

/// Produce the token stream for `Option<&'static str>` from an
/// `Option<String>` captured at macro expansion. Used for every
/// rustdoc-sourced doc field.
fn option_str_token(doc: Option<&String>) -> TokenStream2 {
    if let Some(s) = doc {
        let lit = s.as_str();
        quote! { ::core::option::Option::Some(#lit) }
    } else {
        quote! { ::core::option::Option::None }
    }
}

/// Extract rustdoc from a set of attributes and filter through the
/// `# Agent` section convention. Returns `None` when there's no
/// rustdoc at all; `Some(body)` otherwise — `body` is the `# Agent`
/// section's content if one is present, or the full (trimmed) doc
/// text if not.
///
/// Rustdoc `///` comments lower to `#[doc = "text"]` attributes with
/// one attribute per source line. The text retains its leading space
/// (`/// foo` → `" foo"`), which we preserve verbatim for the joined
/// output — stripping it would alter the agent's view of formatted
/// doc blocks and obscure intentional indentation.
fn extract_agent_doc(attrs: &[Attribute]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let Meta::NameValue(nv) = &attr.meta else {
            continue;
        };
        let Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) = &nv.value
        else {
            continue;
        };
        lines.push(s.value());
    }
    if lines.is_empty() {
        return None;
    }
    let full = lines.join("\n");
    let full_trimmed = full.trim();
    if full_trimmed.is_empty() {
        return None;
    }

    // Scan for a `# Agent` heading (conventional rustdoc section
    // heading, top-level `#` followed by space). Capture everything
    // until the next top-level heading or end-of-doc.
    let mut in_agent = false;
    let mut found_agent = false;
    let mut agent_lines: Vec<&str> = Vec::new();
    for line in full.lines() {
        let trimmed = line.trim_start();
        let starts_h1 = trimmed.starts_with("# ") && !trimmed.starts_with("## ");
        if starts_h1 {
            if in_agent {
                // A new top-level heading ends the Agent section.
                break;
            }
            let heading = trimmed.trim_start_matches('#').trim();
            if heading.eq_ignore_ascii_case("Agent") {
                in_agent = true;
                found_agent = true;
                continue;
            }
            continue;
        }
        if in_agent {
            agent_lines.push(line);
        }
    }

    if found_agent {
        let s = agent_lines.join("\n").trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    } else {
        Some(full_trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::reject_hashmap;
    use syn::parse_str;

    // Issue #232: pin the HashMap rejection so a future field-walker
    // refactor can't silently drop the check (the user-visible
    // failure mode would be a confusing "Schema not implemented"
    // error pointing at HashMap rather than the actionable
    // "use BTreeMap" message). Each fixture covers one shape we
    // expect the rejection to catch — direct, nested in Vec, nested
    // in Option, and inside a fully-qualified path.

    fn err(ty: &str) -> String {
        let parsed: syn::Type = parse_str(ty).expect("test fixture parses");
        reject_hashmap(&parsed)
            .err()
            .unwrap_or_else(|| panic!("expected reject_hashmap to error on {ty}"))
            .to_string()
    }

    #[test]
    fn rejects_direct_hashmap_field() {
        let msg = err("HashMap<String, String>");
        assert!(
            msg.contains("BTreeMap"),
            "error must point to BTreeMap fix, got: {msg}"
        );
        assert!(
            msg.contains("232"),
            "error must reference issue 232, got: {msg}"
        );
    }

    #[test]
    fn rejects_fully_qualified_hashmap() {
        let msg = err("std::collections::HashMap<String, u32>");
        assert!(msg.contains("BTreeMap"));
    }

    #[test]
    fn rejects_hashmap_nested_in_vec() {
        let msg = err("Vec<HashMap<String, String>>");
        assert!(msg.contains("BTreeMap"));
    }

    #[test]
    fn rejects_hashmap_nested_in_option() {
        let msg = err("Option<HashMap<String, String>>");
        assert!(msg.contains("BTreeMap"));
    }

    #[test]
    fn allows_btreemap_field() {
        let parsed: syn::Type =
            parse_str("BTreeMap<String, String>").expect("test setup: BTreeMap type parses");
        assert!(reject_hashmap(&parsed).is_ok());
    }

    #[test]
    fn allows_plain_types() {
        for ty in [
            "u32",
            "String",
            "Vec<u8>",
            "Option<String>",
            "BTreeSet<u64>",
        ] {
            let parsed: syn::Type =
                parse_str(ty).expect("test setup: candidate type parses as syn::Type");
            assert!(
                reject_hashmap(&parsed).is_ok(),
                "rejected {ty} unexpectedly"
            );
        }
    }
}
