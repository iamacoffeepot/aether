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
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, ExprLit, Fields, FnArg,
    GenericArgument, ImplItem, ItemImpl, Lit, Meta, PathArguments, Signature, Type,
    parse_macro_input, spanned::Spanned,
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
        is_stream,
    } = parse_kind_attr(&input.attrs)?;
    if let Data::Union(u) = &input.data {
        return Err(syn::Error::new_spanned(
            u.union_token,
            "Kind derive does not support unions",
        ));
    }
    let is_stream_item = if is_stream {
        quote! { const IS_STREAM: bool = true; }
    } else {
        quote! {}
    };
    let is_stream_byte: u8 = if is_stream { 1 } else { 0 };

    // ADR-0033 wire-shape autodetect: `#[repr(C)]` on the type means
    // the substrate carried it as raw cast bytes (and the user has
    // `#[derive(Pod, Zeroable)]`); anything else is postcard-shaped
    // (and the user has `#[derive(Serialize, Deserialize)]`). The
    // dispatcher in `#[handlers]` calls `Kind::decode_from_bytes` via
    // `Mail::decode_kind::<K>()`; emitting the body per-impl here is
    // what lets that one call site compile against types whose Pod /
    // Deserialize bounds are disjoint.
    let has_repr_c = struct_has_repr_c(&input.attrs);
    let decode_body = if has_repr_c {
        quote! { ::aether_mail::__derive_runtime::decode_cast::<Self>(bytes) }
    } else {
        quote! { ::aether_mail::__derive_runtime::decode_postcard::<Self>(bytes) }
    };
    // Issue #240: encode mirror. Same `#[repr(C)]` autodetect as
    // `decode_body` — a single `Sink::send` call site routes through
    // `Kind::encode_into_bytes`, picking cast or postcard at the
    // kind's derive instead of at every send site.
    let encode_body = if has_repr_c {
        quote! { ::aether_mail::__derive_runtime::encode_cast::<Self>(self) }
    } else {
        quote! { ::aether_mail::__derive_runtime::encode_postcard::<Self>(self) }
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
            // ADR-0064: tag the high 4 bits with `Tag::Kind` so kind
            // ids are distinguishable from mailbox / handle ids by
            // bit pattern alone. The `KIND_DOMAIN` byte prefix still
            // rides the FNV input (ADR-0030) — type info ends up
            // encoded in two independent places that cross-check.
            // Issue 466: `Kind::ID` is typed `KindId`; the wrapper
            // wraps the raw `u64` hash. Wire-format sites that need
            // raw bytes call `.0`; dispatch sites compare `KindId` to
            // `KindId` directly.
            const ID: ::aether_mail::KindId = ::aether_mail::KindId(
                ::aether_mail::with_tag(
                    ::aether_mail::Tag::Kind,
                    ::aether_mail::fnv1a_64_prefixed(
                        ::aether_mail::KIND_DOMAIN,
                        &#canonical_bytes_ident,
                    ),
                ),
            );
            #is_stream_item

            fn decode_from_bytes(bytes: &[u8]) -> ::core::option::Option<Self> {
                #decode_body
            }

            fn encode_into_bytes(&self) -> ::aether_mail::__derive_runtime::Vec<u8> {
                #encode_body
            }
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
                // Issue 469: `KindLabels.kind_id` is now typed
                // `KindId` (matches `Kind::ID`); pass through directly.
                kind_id: <#name as ::aether_mail::Kind>::ID,
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

        // ADR-0068 v0x03: trailing byte after the canonical bytes
        // carries `IS_STREAM`. Canonical bytes (and therefore
        // `Kind::ID`) unchanged from v0x02 — the stream flag is
        // metadata that rides alongside identity, never inside it.
        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds")]
        static #kind_static_ident: [u8; #canonical_len_ident + 2] = {
            let mut out = [0u8; #canonical_len_ident + 2];
            out[0] = 0x03;
            let mut i = 0;
            while i < #canonical_len_ident {
                out[i + 1] = #canonical_bytes_ident[i];
                i += 1;
            }
            out[#canonical_len_ident + 1] = #is_stream_byte;
            out
        };

        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds.labels")]
        static #kind_labels_static_ident: [u8; #labels_len_ident + 1] = {
            let mut out = [0u8; #labels_len_ident + 1];
            // v0x03: `KindLabels` gained `kind_id`, making records
            // self-identifying. Reader pairs by id, not index.
            out[0] = 0x03;
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
        ::aether_mail::__inventory::inventory::submit! {
            ::aether_mail::__inventory::DescriptorEntry {
                name: <#name as ::aether_mail::Kind>::NAME,
                schema: &#schema_static_ident,
                is_stream: <#name as ::aether_mail::Kind>::IS_STREAM,
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
                let field_exprs = unnamed.unnamed.iter().map(|f| field_label_node_expr(&f.ty));
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
                let field_node_exprs = named.named.iter().map(|f| field_label_node_expr(&f.ty));
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
    for v in &data.variants {
        for f in v.fields.iter() {
            reject_hashmap(&f.ty)?;
        }
    }

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
    is_stream: bool,
}

fn parse_kind_attr(attrs: &[Attribute]) -> syn::Result<KindAttr> {
    for attr in attrs {
        if !attr.path().is_ident("kind") {
            continue;
        }
        let mut name: Option<String> = None;
        let mut is_stream = false;
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
            if meta.path.is_ident("stream") {
                // Flag-shaped — no `= value`. ADR-0021 + ADR-0068:
                // marks this kind as a substrate-published event
                // stream that components subscribe to via the per-kind
                // subscriber set. Drives `<K as Kind>::IS_STREAM` and
                // the trailing byte in the `aether.kinds` v0x03 wire
                // format that the substrate reads to gate auto-
                // subscribe.
                is_stream = true;
                return Ok(());
            }
            Err(meta.error("expected `name = \"...\"` or `stream`"))
        })?;
        if let Some(name) = name {
            return Ok(KindAttr { name, is_stream });
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

// ADR-0033 phase 3: `#[handlers]` on an `impl Component for C` block
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
//       `aether_component::export!()` in the cdylib root crate, NOT
//       here. Sections only land where `export!()` runs (the cdylib
//       root); transitive rlib pulls of a `#[handlers]`-using crate
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

#[proc_macro_attribute]
pub fn handlers(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[handlers] takes no arguments",
        )
        .to_compile_error()
        .into();
    }
    let item = parse_macro_input!(item as ItemImpl);
    match expand_handlers(item) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn handler(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    // Real logic runs inside `#[handlers]` (the enclosing impl-block
    // attribute scans for #[handler] markers). This standalone shim
    // only exists so rustc accepts `#[handler]` syntactically outside
    // macro expansion and so rust-analyzer doesn't redline it.
    syn::Error::new(
        proc_macro2::Span::call_site(),
        "#[handler] may only appear inside a `#[handlers] impl Component for T` block",
    )
    .to_compile_error()
    .into()
}

#[proc_macro_attribute]
pub fn fallback(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    // Same story as `#[handler]` — marker attribute consumed by the
    // enclosing `#[handlers]` scan. Standalone invocation is a
    // compile-time error.
    syn::Error::new(
        proc_macro2::Span::call_site(),
        "#[fallback] may only appear inside a `#[handlers] impl Component for T` block",
    )
    .to_compile_error()
    .into()
}

struct HandlerFn {
    method: syn::ImplItemFn,
    kind_ty: Type,
    agent_doc: Option<String>,
}

struct FallbackFn {
    method: syn::ImplItemFn,
    agent_doc: Option<String>,
}

fn expand_handlers(item: ItemImpl) -> syn::Result<TokenStream2> {
    if item.trait_.is_none() {
        return Err(syn::Error::new_spanned(
            &item,
            "#[handlers] must wrap `impl Component for T` — not an inherent impl",
        ));
    }
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

    for impl_item in item.items {
        match impl_item {
            ImplItem::Type(it) if it.ident == "Kinds" => {
                return Err(syn::Error::new_spanned(
                    it,
                    "#[handlers] synthesizes `type Kinds` from the #[handler] methods; remove this declaration",
                ));
            }
            ImplItem::Fn(mut f) => {
                let name = f.sig.ident.to_string();
                let handler_attr_idx = f.attrs.iter().position(|a| a.path().is_ident("handler"));
                let fallback_attr_idx = f.attrs.iter().position(|a| a.path().is_ident("fallback"));

                if handler_attr_idx.is_some() && fallback_attr_idx.is_some() {
                    return Err(syn::Error::new_spanned(
                        &f,
                        "method cannot be both #[handler] and #[fallback]",
                    ));
                }

                if let Some(idx) = handler_attr_idx {
                    let kind_ty = extract_handler_kind_type(&f.sig)?;
                    let agent_doc = extract_agent_doc(&f.attrs);
                    f.attrs.remove(idx);
                    handlers.push(HandlerFn {
                        method: f,
                        kind_ty,
                        agent_doc,
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
                } else if matches!(name.as_str(), "on_replace" | "on_drop" | "on_rehydrate") {
                    lifecycle_methods.push(f);
                } else if name == "receive" {
                    return Err(syn::Error::new_spanned(
                        &f,
                        "#[handlers] synthesizes `fn receive`; remove this definition",
                    ));
                } else {
                    helpers.push(f);
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unexpected item in #[handlers] impl (only fns and the synthesized `type Kinds` are allowed)",
                ));
            }
        }
    }

    let init_method = init_method.ok_or_else(|| {
        syn::Error::new_spanned(
            self_ty,
            "#[handlers] requires `fn init(ctx: &mut InitCtx<'_>) -> Self`",
        )
    })?;

    if handlers.is_empty() && fallback.is_none() {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[handlers] requires at least one #[handler] method or a #[fallback] method",
        ));
    }

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
    let wrapped_init = init_method;
    let dispatch_body = build_dispatch_body(&handlers, fallback.as_ref());

    let handler_methods_tokens = handlers.iter().map(|h| &h.method);
    let fallback_method_tokens = fallback.as_ref().map(|f| &f.method);
    let helper_methods_tokens = helpers.iter();

    let inputs_manifest_consts =
        build_inputs_manifest_consts(&handlers, fallback.as_ref(), &component_doc);
    let kind_retention_statics = build_kinds_section_retention_statics(self_ty, &handlers);

    Ok(quote! {
        impl #impl_generics #trait_path for #self_ty #where_clause {
            #wrapped_init

            #(#lifecycle_methods)*
        }

        impl #impl_generics #self_ty #where_clause {
            #[doc(hidden)]
            pub fn __aether_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_component::Ctx<'_>,
                __aether_mail: ::aether_component::Mail<'_>,
            ) -> u32 {
                #dispatch_body
            }

            #inputs_manifest_consts

            #(#handler_methods_tokens)*
            #fallback_method_tokens
            #(#helper_methods_tokens)*
        }

        #kind_retention_statics
    })
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

/// Soft validation that a `#[fallback]` method's signature is shaped
/// for `Mail<'_>`. We don't do deep type equality against
/// `::aether_component::Mail<'_>` — the synthesized dispatcher's call
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
        // `Mail::kind()` returns the raw `u64` the FFI carried; `Kind::ID`
        // is typed `KindId` post-issue 466, so we drop into `.0` for the
        // comparison.
        quote! {
            if __aether_kind == <#k as ::aether_component::__macro_internals::Kind>::ID.0 {
                if let ::core::option::Option::Some(__aether_decoded) =
                    __aether_mail.decode_kind::<#k>()
                {
                    self.#method(__aether_ctx, __aether_decoded);
                }
                return ::aether_component::DISPATCH_HANDLED;
            }
        }
    });

    let tail = match fallback {
        Some(f) => {
            let method = &f.method.sig.ident;
            quote! {
                self.#method(__aether_ctx, __aether_mail);
                ::aether_component::DISPATCH_HANDLED
            }
        }
        None => quote! { ::aether_component::DISPATCH_UNKNOWN_KIND },
    };

    quote! {
        let __aether_kind = __aether_mail.kind();
        __aether_ctx.__set_reply_to(__aether_mail.reply_to());
        #( #arms )*
        #tail
    }
}

/// Emit two associated consts inside the component's inherent impl —
/// `__AETHER_INPUTS_MANIFEST_LEN: usize` and
/// `__AETHER_INPUTS_MANIFEST: [u8; …LEN]` — carrying the
/// concatenated `aether.kinds.inputs` record bytes. Each record is
/// `[INPUTS_SECTION_VERSION (0x01), ..postcard(InputsRecord)..]`,
/// assembled at const-eval via the hub-protocol const-fn encoders.
/// `aether_component::export!()` reads these consts and emits the
/// `#[unsafe(link_section = "aether.kinds.inputs")]` static in the
/// cdylib root crate. Keeping the section emission out of this macro
/// is what prevents the section from stacking when a `#[handlers]`-
/// using crate is pulled in as a wasm32 rlib by another cdylib (a
/// rlib that doesn't call `export!()` contributes no section bytes).
fn build_inputs_manifest_consts(
    handlers: &[HandlerFn],
    fallback: Option<&FallbackFn>,
    component_doc: &Option<String>,
) -> TokenStream2 {
    let mut len_terms: Vec<TokenStream2> = Vec::new();
    let mut copy_blocks: Vec<TokenStream2> = Vec::new();

    for h in handlers {
        let k = &h.kind_ty;
        let doc_expr = option_str_token(&h.agent_doc);
        // `inputs_handler_len` / `write_inputs_handler` take a raw `u64`
        // for the wire bytes; `Kind::ID` is `KindId` post-issue 466 so
        // we drop into `.0` here.
        len_terms.push(quote! {
            (1 + ::aether_component::__macro_internals::canonical::inputs_handler_len(
                <#k as ::aether_component::__macro_internals::Kind>::ID.0,
                <#k as ::aether_component::__macro_internals::Kind>::NAME,
                #doc_expr,
            ))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_component::__macro_internals::canonical::inputs_handler_len(
                        <#k as ::aether_component::__macro_internals::Kind>::ID.0,
                        <#k as ::aether_component::__macro_internals::Kind>::NAME,
                        #doc_expr,
                    );
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_component::__macro_internals::canonical::write_inputs_handler::<REC_LEN>(
                        <#k as ::aether_component::__macro_internals::Kind>::ID.0,
                        <#k as ::aether_component::__macro_internals::Kind>::NAME,
                        #doc_expr,
                    );
                out[pos] = 0x01;
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
        let doc_expr = option_str_token(&f.agent_doc);
        len_terms.push(quote! {
            (1 + ::aether_component::__macro_internals::canonical::inputs_fallback_len(#doc_expr))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_component::__macro_internals::canonical::inputs_fallback_len(#doc_expr);
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_component::__macro_internals::canonical::write_inputs_fallback::<REC_LEN>(#doc_expr);
                out[pos] = 0x01;
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

    if let Some(doc) = component_doc.as_ref() {
        let doc_lit = doc.as_str();
        len_terms.push(quote! {
            (1 + ::aether_component::__macro_internals::canonical::inputs_component_len(#doc_lit))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_component::__macro_internals::canonical::inputs_component_len(#doc_lit);
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_component::__macro_internals::canonical::write_inputs_component::<REC_LEN>(#doc_lit);
                out[pos] = 0x01;
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
/// `#[handlers]` emits it here, in the consumer's own compilation
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
fn build_kinds_section_retention_statics(self_ty: &Type, handlers: &[HandlerFn]) -> TokenStream2 {
    let self_ty_hint = type_hint(self_ty);

    let statics = handlers.iter().enumerate().map(|(idx, h)| {
        let k = &h.kind_ty;
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
            static #schema_ident: ::aether_component::__macro_internals::SchemaType =
                <#k as ::aether_component::__macro_internals::Schema>::SCHEMA;
            const #len_ident: usize =
                ::aether_component::__macro_internals::canonical::canonical_len_kind(
                    <#k as ::aether_component::__macro_internals::Kind>::NAME,
                    &#schema_ident,
                );
            const #bytes_ident: [u8; #len_ident] =
                ::aether_component::__macro_internals::canonical::canonical_serialize_kind::<#len_ident>(
                    <#k as ::aether_component::__macro_internals::Kind>::NAME,
                    &#schema_ident,
                );
            // ADR-0068 v0x03: trailing byte after canonical bytes
            // carries `<K as Kind>::IS_STREAM`. Same wire shape as
            // `expand_kind`'s primary emission so retention records
            // (when this kind lives in a dependency rlib) and the
            // primary records pair cleanly by id without disagreeing
            // on the trailing flag.
            #[cfg(target_arch = "wasm32")]
            #[used]
            #[unsafe(link_section = "aether.kinds")]
            static #section_ident: [u8; #len_ident + 2] = {
                let mut out = [0u8; #len_ident + 2];
                out[0] = 0x03;
                let mut i = 0;
                while i < #len_ident {
                    out[i + 1] = #bytes_ident[i];
                    i += 1;
                }
                out[#len_ident + 1] = if <#k as ::aether_component::__macro_internals::Kind>::IS_STREAM { 1 } else { 0 };
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
            static #labels_static_ident: ::aether_component::__macro_internals::KindLabels =
                ::aether_component::__macro_internals::KindLabels {
                    // Issue 469: `KindLabels.kind_id` is typed
                    // `KindId` end-to-end; pass through directly.
                    kind_id: <#k as ::aether_component::__macro_internals::Kind>::ID,
                    kind_label: ::aether_component::__macro_internals::Cow::Borrowed(
                        match <#k as ::aether_component::__macro_internals::Schema>::LABEL {
                            ::core::option::Option::Some(s) => s,
                            ::core::option::Option::None => "",
                        },
                    ),
                    root: <#k as ::aether_component::__macro_internals::Schema>::LABEL_NODE,
                };
            const #labels_len_ident: usize =
                ::aether_component::__macro_internals::canonical::canonical_len_labels(
                    &#labels_static_ident,
                );
            const #labels_bytes_ident: [u8; #labels_len_ident] =
                ::aether_component::__macro_internals::canonical::canonical_serialize_labels::<#labels_len_ident>(
                    &#labels_static_ident,
                );
            #[cfg(target_arch = "wasm32")]
            #[used]
            #[unsafe(link_section = "aether.kinds.labels")]
            static #labels_section_ident: [u8; #labels_len_ident + 1] = {
                let mut out = [0u8; #labels_len_ident + 1];
                out[0] = 0x03;
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
fn option_str_token(doc: &Option<String>) -> TokenStream2 {
    match doc {
        Some(s) => {
            let lit = s.as_str();
            quote! { ::core::option::Option::Some(#lit) }
        }
        None => quote! { ::core::option::Option::None },
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
        let parsed: syn::Type = parse_str("BTreeMap<String, String>").unwrap();
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
            let parsed: syn::Type = parse_str(ty).unwrap();
            assert!(
                reject_hashmap(&parsed).is_ok(),
                "rejected {ty} unexpectedly"
            );
        }
    }
}
