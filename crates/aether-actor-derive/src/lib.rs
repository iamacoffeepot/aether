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
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, ExprLit, Fields, FnArg,
    GenericArgument, ImplItem, Item, ItemImpl, ItemMod, Lit, Meta, PathArguments, Signature, Type,
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
    // dispatcher in `#[actor]` calls `Kind::decode_from_bytes` via
    // `Mail::decode_kind::<K>()`; emitting the body per-impl here is
    // what lets that one call site compile against types whose Pod /
    // Deserialize bounds are disjoint.
    let has_repr_c = struct_has_repr_c(&input.attrs);
    let decode_body = if has_repr_c {
        quote! { ::aether_data::__derive_runtime::decode_cast::<Self>(bytes) }
    } else {
        quote! { ::aether_data::__derive_runtime::decode_postcard::<Self>(bytes) }
    };
    // Issue #240: encode mirror. Same `#[repr(C)]` autodetect as
    // `decode_body` — a single `Sink::send` call site routes through
    // `Kind::encode_into_bytes`, picking cast or postcard at the
    // kind's derive instead of at every send site.
    let encode_body = if has_repr_c {
        quote! { ::aether_data::__derive_runtime::encode_cast::<Self>(self) }
    } else {
        quote! { ::aether_data::__derive_runtime::encode_postcard::<Self>(self) }
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
            #is_stream_item

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
        ::aether_data::__inventory::inventory::submit! {
            ::aether_data::__inventory::DescriptorEntry {
                name: <#name as ::aether_data::Kind>::NAME,
                schema: &#schema_static_ident,
                is_stream: <#name as ::aether_data::Kind>::IS_STREAM,
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
                    let fname = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
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
        for f in v.fields.iter() {
            reject_hashmap(&f.ty)?;
        }
    }

    let variant_entries = data.variants.iter().enumerate().map(|(idx, v)| {
        let name = v.ident.to_string();
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
                    let fname = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
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

/// Outer attribute on an `impl WasmActor for X` (or `impl Component for X`)
/// block. Reads the `#[handler]` / `#[fallback]` methods inside, then emits:
///
/// - One `impl HandlesKind<K> for X` per handler kind (gates type-driven
///   sender bounds — ADR-0075).
/// - The dispatch table inherent method `__aether_dispatch` that the
///   `export!` shim's `receive_p32` calls.
/// - The `aether.kinds.inputs` manifest consts (substrate reads them via
///   the wasm custom section the cdylib's `export!` pins in).
/// - The `Actor`-trait const re-routing (NAMESPACE / FRAME_BARRIER from
///   the impl block flow into a sibling `impl Actor`).
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
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("skip_markers") {
            opts.skip_markers = true;
            Ok(())
        } else {
            Err(meta.error("unrecognised #[actor] argument; only `skip_markers` is supported"))
        }
    });
    syn::parse::Parser::parse2(parser, attr)?;
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
    let feature = match parse_bridge_attr(attr) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };
    let item = parse_macro_input!(item as ItemMod);
    match expand_bridge(item, feature) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Parse the optional `feature = "name"` argument on `#[bridge]`.
/// Empty attr returns `None` (the default no-feature mode); anything
/// else parses as a `syn::MetaNameValue` whose path must be `feature`
/// and value must be a string literal.
fn parse_bridge_attr(attr: TokenStream) -> syn::Result<Option<String>> {
    if attr.is_empty() {
        return Ok(None);
    }
    let meta: syn::MetaNameValue = syn::parse(attr)?;
    if !meta.path.is_ident("feature") {
        return Err(syn::Error::new_spanned(
            &meta.path,
            "#[bridge] only accepts `feature = \"name\"`",
        ));
    }
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(s),
        ..
    }) = meta.value
    else {
        return Err(syn::Error::new_spanned(
            &meta.path,
            "#[bridge(feature = ...)] expects a string literal",
        ));
    };
    Ok(Some(s.value()))
}

fn expand_bridge(mut item_mod: ItemMod, feature: Option<String>) -> syn::Result<TokenStream2> {
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
    let (
        self_ty,
        type_ident,
        generics,
        namespace_expr,
        frame_barrier_expr,
        handler_kinds,
        catch_all,
    ) = {
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
        let mut frame_barrier_expr: Option<Expr> = None;
        for impl_item in &actor_impl.items {
            match impl_item {
                ImplItem::Fn(f) if f.attrs.iter().any(attr_is_handler) => {
                    let (kind_ty, _is_slice) = extract_native_actor_handler_kind(&f.sig)?;
                    handler_kinds.push(kind_ty);
                }
                ImplItem::Fn(f) if f.attrs.iter().any(attr_is_fallback) => {
                    has_fallback = true;
                }
                ImplItem::Const(c) => {
                    if c.ident == "NAMESPACE" {
                        namespace_expr = Some(c.expr.clone());
                    } else if c.ident == "FRAME_BARRIER" {
                        frame_barrier_expr = Some(c.expr.clone());
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
        // Issue 576: bridge-wrapped actors come in two flavours —
        // strict typed receiver (only #[handler]s) or catch-all cap
        // (only #[fallback]). Hybrid is rejected for the same reason
        // the inner expander rejects it (the always-on blanket
        // `HandlesKind<K>` would overlap with per-handler impls, and
        // strict receivers shouldn't silently swallow unknown kinds).
        if !handler_kinds.is_empty() && has_fallback {
            return Err(syn::Error::new_spanned(
                actor_impl,
                "#[bridge]'s inner #[actor] block cannot mix #[handler] and #[fallback] — \
                 pick one shape: strict typed receiver (only #[handler]s) or catch-all cap \
                 (only #[fallback])",
            ));
        }
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
            frame_barrier_expr,
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
    let frame_barrier_const = frame_barrier_expr.map(|expr| {
        quote! { const FRAME_BARRIER: bool = #expr; }
    });
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
    let singleton_marker = quote! {
        impl #impl_generics ::aether_actor::Singleton for #self_ty #where_clause {}
    };
    let actor_marker = quote! {
        impl #impl_generics ::aether_actor::Actor for #self_ty #where_clause {
            const NAMESPACE: &'static str = #namespace_expr;
            #frame_barrier_const
        }
    };
    // Issue 576: catch-all caps (only #[fallback]) emit one blanket
    // `impl<K: Kind> HandlesKind<K> for X {}` so typed sends compile
    // for every K. Strict receivers (only #[handler]s) keep
    // per-handler impls. Mixed shape was already rejected above.
    let handles_kind_markers: Vec<TokenStream2> = if catch_all {
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
        for attr in actor_impl_mut.attrs.iter_mut() {
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
    let mod_attrs = std::mem::take(&mut item_mod.attrs);
    Ok(quote! {
        #stub_and_reexport
        #singleton_marker
        #actor_marker
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
        "#[handler] may only appear inside a `#[actor] impl WasmActor for T` block",
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
        "#[fallback] may only appear inside a `#[actor] impl WasmActor for T` block",
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
/// Issue 552 stage 0: explicit opt-in marker that an actor type is
/// the sole instance of its kind in a chassis. The trait itself is a
/// simple marker — no methods. Future stages may grow `Singleton`
/// into a richer trait (e.g. an associated `unique_name() -> &str`)
/// at which point this derive emits the additional bodies; today
/// it's just the marker.
///
/// Kept as `#[derive(Singleton)]` rather than auto-emitted by
/// `#[actor]` so opting OUT (multi-instance actors, when stage 5+
/// introduces them) is non-breaking — the absence of the derive
/// becomes the opt-out signal instead of a new attribute syntax.
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
        syn::Fields::Named(fields) => {
            for field in fields.named.iter_mut() {
                let already_cfg = field.attrs.iter().any(|a| a.path().is_ident("cfg"));
                if !already_cfg {
                    field
                        .attrs
                        .push(syn::parse_quote!(#[cfg(not(target_arch = "wasm32"))]));
                }
            }
        }
        syn::Fields::Unnamed(fields) => {
            for field in fields.unnamed.iter_mut() {
                let already_cfg = field.attrs.iter().any(|a| a.path().is_ident("cfg"));
                if !already_cfg {
                    field
                        .attrs
                        .push(syn::parse_quote!(#[cfg(not(target_arch = "wasm32"))]));
                }
            }
        }
        syn::Fields::Unit => {
            // Marker structs: nothing to gate.
        }
    }
    quote! { #item }.into()
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

fn expand_handlers(item: ItemImpl, opts: ActorOpts) -> syn::Result<TokenStream2> {
    if let Some((_, trait_path, _)) = item.trait_.as_ref() {
        // Pattern-match the trait path's last identifier so the macro
        // works regardless of the user's import style — bare
        // `WasmActor` / `NativeActor`, `aether_actor::WasmActor`,
        // `aether_substrate::NativeActor`, etc. all resolve here.
        let last = trait_path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        match last.as_str() {
            "NativeActor" => expand_native_actor_trait(item, opts),
            // `WasmActor` is the post-552 trait name; `Component` is
            // the back-compat alias retained until stage 4.
            "WasmActor" | "Component" => {
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
                    "#[actor] expects `impl WasmActor for X`, `impl NativeActor for X`, or \
                     `impl Component for X` (back-compat alias) — got `{other}`",
                ),
            )),
        }
    } else {
        if opts.skip_markers {
            return Err(syn::Error::new_spanned(
                &item.self_ty,
                "#[actor(skip_markers)] is only meaningful on \
                 `impl NativeActor for X` blocks wrapped by `#[bridge]`",
            ));
        }
        // Inherent `impl X { … }` is the legacy native-cap shape used
        // by post-545 capabilities (LogCapability et al.). Stays
        // available through stage 1; stage 2 migrates caps onto the
        // `impl NativeActor for X` shape so this arm retires.
        expand_native_actor(item)
    }
}

/// Match `#[handler]`, `#[crate::handler]`, or `#[aether_data::handler]` —
/// any path whose last segment is `handler`. Bare `is_ident("handler")`
/// only matches the unqualified form, so qualified-path tests like
/// `#[aether_data::handler]` would skip the macro silently.
fn attr_is_handler(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "handler")
}

/// Same logic for `#[fallback]`.
fn attr_is_fallback(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "fallback")
}

/// Wasm-actor expansion — `#[actor] impl WasmActor for X` (or
/// the back-compat `impl Component for X`). Emits the full wasm
/// surface: dispatch table referencing `aether_actor::WasmCtx<'_>`,
/// init wrapper, `aether.kinds.inputs` manifest consts, kind retention
/// statics, plus the `HandlesKind<K>` and `Actor` impls common to both
/// shapes.
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
    // Issue 525 Phase 1B: pass-through trait consts (today: NAMESPACE,
    // FRAME_BARRIER) so each component declares them inside its
    // `#[actor] impl WasmActor for C` block alongside `init` /
    // `#[handler]` methods.
    let mut consts: Vec<syn::ImplItemConst> = Vec::new();

    for impl_item in item.items {
        match impl_item {
            ImplItem::Type(it) if it.ident == "Kinds" => {
                return Err(syn::Error::new_spanned(
                    it,
                    "#[actor] synthesizes `type Kinds` from the #[handler] methods; remove this declaration",
                ));
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
                        "#[actor] synthesizes `fn receive`; remove this definition",
                    ));
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

    let init_method = init_method.ok_or_else(|| {
        syn::Error::new_spanned(
            self_ty,
            "#[actor] requires `fn init(ctx: &mut InitCtx<'_>) -> Result<Self, BootError>`",
        )
    })?;

    if handlers.is_empty() && fallback.is_none() {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] requires at least one #[handler] method or a #[fallback] method",
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

    // Issue 525 Phase 4: trait consts (NAMESPACE, FRAME_BARRIER) live
    // on the `Actor` super-trait, not `Component` / `WasmActor`. Route
    // any const items the user declared inside `#[actor] impl
    // Component for X` to a sibling `impl ::aether_actor::Actor`
    // block so satisfying `WasmActor: Actor` works without making the
    // user split the impl manually.
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

    Ok(quote! {
        #actor_impl

        #(#handles_kind_impls)*

        impl #impl_generics #trait_path for #self_ty #where_clause {
            #wrapped_init

            #(#lifecycle_methods)*
        }

        impl #impl_generics #self_ty #where_clause {
            #[doc(hidden)]
            pub fn __aether_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_>,
                __aether_mail: ::aether_actor::Mail<'_>,
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

/// Native-actor expansion — `#[actor] impl X` (inherent impl).
/// Used for chassis cap facades in `aether-kinds` whose `NativeActor`
/// impl lives in `aether-substrate` but whose marker / handler list
/// must stay wasm-importable.
///
/// Emits:
///   - `impl HandlesKind<K> for X` per `#[handler]` method.
///   - A `__dispatch(&mut self, kind: u64, payload: &[u8]) -> Option<()>`
///     fn that decodes payload and calls the matching handler.
///     Substrate's `NativeActor::boot` calls this from the dispatcher
///     thread loop.
///   - The original handler methods, attribute-stripped, as inherent
///     methods (so they're callable from `__dispatch` and from the
///     backend trait impl that delegates to them).
///
/// Native handlers take `(&mut self, kind: K)` — no ctx parameter.
/// Chassis caps that need to send mail or hold runtime state do so
/// through the backend trait impl; the cap's handler body is just
/// `self.backend.on_X(kind)` delegation.
fn expand_native_actor(item: ItemImpl) -> syn::Result<TokenStream2> {
    let self_ty = &item.self_ty;
    let generics = &item.generics;
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();

    let mut handlers: Vec<NativeHandlerFn> = Vec::new();
    let mut helpers: Vec<syn::ImplItemFn> = Vec::new();

    for impl_item in item.items {
        match impl_item {
            ImplItem::Fn(mut f) => {
                let handler_attr_idx = f.attrs.iter().position(attr_is_handler);
                if f.attrs.iter().any(attr_is_fallback) {
                    return Err(syn::Error::new_spanned(
                        &f,
                        "#[fallback] is not supported on native chassis-cap impls (#[actor] on inherent impl) — \
                         every kind a chassis cap accepts is declared via #[handler]",
                    ));
                }
                if let Some(idx) = handler_attr_idx {
                    let (kind_ty, takes_sender, is_slice) =
                        extract_native_handler_kind_type(&f.sig)?;
                    f.attrs.remove(idx);
                    handlers.push(NativeHandlerFn {
                        method: f,
                        kind_ty,
                        takes_sender,
                        is_slice,
                    });
                } else {
                    helpers.push(f);
                }
            }
            ImplItem::Const(_) => {
                helpers.push(syn::ImplItemFn {
                    attrs: Vec::new(),
                    vis: syn::Visibility::Inherited,
                    defaultness: None,
                    sig: syn::parse_quote!(fn __unused_native_const_marker()),
                    block: syn::parse_quote!({}),
                });
                // We don't actually keep the const — unsupported on native impls
                return Err(syn::Error::new_spanned(
                    self_ty,
                    "associated consts are not supported on native chassis-cap impls (#[actor] on inherent impl); \
                     declare them on the standalone `Actor` impl instead",
                ));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unexpected item in #[actor] inherent impl (only fns are allowed)",
                ));
            }
        }
    }

    if handlers.is_empty() {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] inherent impl requires at least one #[handler] method",
        ));
    }

    let handles_kind_impls = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        quote! {
            impl #impl_generics ::aether_actor::HandlesKind<#kind_ty>
                for #self_ty #where_clause {}
        }
    });

    // Dispatch body: one if-arm per handler. Each arm decodes the
    // payload (returning early on decode failure for the matched kind
    // — substrate-side dispatcher logs the miss separately) and calls
    // the inherent handler method by its original ident.
    //
    // Two call shapes:
    //   - `takes_sender = true` → forward the dispatcher's `sender`
    //     argument as the second arg. Used by reply-bearing caps
    //     (Handle, Audio, Io, Net, ...) — issue 533 PR D1.
    //   - `takes_sender = false` → drop sender on the floor.
    //     Fire-and-forget caps (Log) keep the 2-arg signature.
    let dispatch_arms = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        let method_ident = &h.method.sig.ident;
        let call = if h.takes_sender {
            quote! { self.#method_ident(__sender, __decoded); }
        } else {
            quote! { self.#method_ident(__decoded); }
        };
        if h.is_slice {
            // Slice handler — payload is `count * size_of::<K>()`
            // contiguous bytes (ADR-0019 batch wire). Cast to
            // `&[K]` for the handler. Only meaningful for cast-shape
            // kinds; postcard kinds reject `&[K]` at the macro
            // boundary because there's no batched postcard wire.
            quote! {
                if kind == <#kind_ty as ::aether_data::Kind>::ID.0 {
                    if let Some(__decoded) =
                        ::aether_data::__derive_runtime::decode_cast_slice::<#kind_ty>(payload)
                    {
                        #call
                        return Some(());
                    }
                    return None;
                }
            }
        } else {
            quote! {
                if kind == <#kind_ty as ::aether_data::Kind>::ID.0 {
                    if let Some(__decoded) = <#kind_ty as ::aether_data::Kind>::decode_from_bytes(payload) {
                        #call
                        return Some(());
                    }
                    return None;
                }
            }
        }
    });

    let handler_methods_tokens = handlers.iter().map(|h| &h.method);
    let helper_methods_tokens = helpers.iter();

    Ok(quote! {
        #(#handles_kind_impls)*

        impl #impl_generics ::aether_actor::Dispatch for #self_ty #where_clause {
            fn __dispatch(
                &mut self,
                __sender: ::aether_data::ReplyTo,
                kind: u64,
                payload: &[u8],
            ) -> Option<()> {
                // `__sender` is shadowed-bind so dispatch arms that
                // forward it stay readable. Underscore prefix avoids
                // shadowing user identifiers when no handler takes it.
                let _ = &__sender;
                #(#dispatch_arms)*
                None
            }
        }

        impl #impl_generics #self_ty #where_clause {
            #(#handler_methods_tokens)*
            #(#helper_methods_tokens)*
        }
    })
}

/// Issue 552 stage 1: expansion for `#[actor] impl NativeActor for X`
/// — the new native chassis-cap shape. Per-handler ctx + `&self`
/// (Arc-shared) + typed `init`. Mirrors `expand_wasm_actor`'s shape
/// across the wasm/native split.
///
/// Emits, all rooted in the consumer crate's namespace:
///   - `impl Actor for X` carrying the user-declared `const NAMESPACE`
///     / `const FRAME_BARRIER` (extracted from the impl block so the
///     `NativeActor: Actor` supertrait bound is satisfied).
///   - `impl HandlesKind<K> for X` per `#[handler]` method — the
///     compile-time gate `Sender::send::<R, K>` consults.
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
    let mut fallback: Option<NativeFallbackFn> = None;
    let mut helpers: Vec<syn::ImplItemFn> = Vec::new();
    let mut consts: Vec<syn::ImplItemConst> = Vec::new();

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
                    let (kind_ty, is_slice) = extract_native_actor_handler_kind(&f.sig)?;
                    f.attrs.remove(idx);
                    handlers.push(NativeActorHandlerFn {
                        method: f,
                        kind_ty,
                        is_slice,
                    });
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

    // Issue 576: native actors come in two flavours — strict typed
    // receiver (only #[handler]s) or catch-all cap (only #[fallback]).
    // Hybrid (typed + fallback as runtime safety net) is forbidden:
    // strict receivers shouldn't silently swallow unknown kinds, and
    // the type-system catch-all blanket `HandlesKind<K>` would overlap
    // with per-handler impls anyway.
    if !handlers.is_empty() && fallback.is_some() {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor cannot mix #[handler] and #[fallback] — \
             pick one shape: strict typed receiver (only #[handler]s) or catch-all cap \
             (only #[fallback])",
        ));
    }
    if handlers.is_empty() && fallback.is_none() {
        return Err(syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor requires at least one #[handler] method \
             or a #[fallback] method",
        ));
    }

    // `NAMESPACE` / `FRAME_BARRIER` are declared on the supertrait
    // `Actor`, but the user wrote them inside `impl NativeActor for X`
    // for the symmetric authoring shape. Route the consts onto a
    // sibling `impl Actor for X` block so satisfying the supertrait
    // bound works without making the user split the impl.
    //
    // `skip_markers` (issue 565): when `#[bridge]` wraps a cfg-gated
    // `mod native` containing the actor block, it emits the always-on
    // `Actor` + `HandlesKind` impls itself as siblings of the mod and
    // rewrites this `#[actor]` to `#[actor(skip_markers)]` so this
    // expansion does not duplicate them. The native-only impls below
    // still emit unchanged.
    let const_tokens = consts.iter();
    let actor_impl = if opts.skip_markers || consts.is_empty() {
        quote! {}
    } else {
        quote! {
            impl #impl_generics ::aether_actor::Actor for #self_ty #where_clause {
                #(#const_tokens)*
            }
        }
    };

    // Issue 576: catch-all caps (only #[fallback], no #[handler]s) get
    // a single blanket `impl<K: Kind> HandlesKind<K> for X {}` so any
    // typed `ctx.actor::<X>().send(&payload)` compiles for every K.
    // The orphan rule allows this because the self type is local. For
    // strict receivers (only #[handler]s) we keep per-handler impls.
    let handles_kind_impls: Vec<TokenStream2> = if opts.skip_markers {
        Vec::new()
    } else if fallback.is_some() {
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

    let dispatch_arms = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        let method_ident = &h.method.sig.ident;
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
                        self.#method_ident(__aether_ctx, __aether_decoded);
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
                        self.#method_ident(__aether_ctx, __aether_decoded);
                        return ::core::option::Option::Some(());
                    }
                    return ::core::option::Option::None;
                }
            }
        }
    });

    let handler_methods: Vec<&syn::ImplItemFn> = handlers.iter().map(|h| &h.method).collect();
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
        quote! {
            fn __aether_dispatch_fallback(
                &self,
                __aether_ctx: &mut ::aether_substrate::NativeCtx<'_>,
                __aether_env: &::aether_substrate::capability::Envelope,
            ) -> bool {
                self.#method_ident(__aether_ctx, __aether_env);
                true
            }
        }
    });

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

        #[cfg(not(target_arch = "wasm32"))]
        impl #impl_generics #trait_path for #self_ty #where_clause {
            #config_type
            #init_method
        }

        #[cfg(not(target_arch = "wasm32"))]
        impl #impl_generics ::aether_substrate::NativeDispatch for #self_ty #where_clause {
            fn __aether_dispatch_envelope(
                &self,
                __aether_ctx: &mut ::aether_substrate::NativeCtx<'_>,
                __aether_kind: ::aether_substrate::mail::KindId,
                __aether_payload: &[u8],
            ) -> ::core::option::Option<()> {
                #(#dispatch_arms)*
                ::core::option::Option::None
            }

            #fallback_dispatch_override
        }

        #[cfg(not(target_arch = "wasm32"))]
        impl #impl_generics #self_ty #where_clause {
            #(#handler_methods)*
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
}

/// Issue 576: native-side `#[fallback]` collected on a
/// `#[actor] impl NativeActor for X` block. Mirrors the wasm-side
/// [`FallbackFn`] but the native handler signature pivots on
/// [`Envelope`] — it carries the kind id, kind name, origin, sender,
/// and payload in one borrow so catch-all caps (broadcast, future
/// hub-as-actor) can lift the whole envelope into a downstream call
/// without rebuilding fields the trampoline already has.
///
/// [`Envelope`]: aether_substrate::capability::Envelope
struct NativeFallbackFn {
    method: syn::ImplItemFn,
}

/// Validate a native `#[fallback]` method signature. Required shape:
/// `(&self, ctx: &mut NativeCtx<'_>, env: &Envelope)`. The third
/// argument's exact type isn't checked here — the synthesized
/// override calls `self.<fallback>(ctx, env)` and the user's fn body
/// will type-error against `&Envelope` if they wrote the wrong
/// parameter type.
fn validate_native_fallback_sig(sig: &Signature) -> syn::Result<()> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[fallback] on `impl NativeActor for X` must have signature \
             `(&self, ctx: &mut NativeCtx<'_>, env: &Envelope)`",
        ));
    }
    let first = &sig.inputs[0];
    let FnArg::Receiver(recv) = first else {
        return Err(syn::Error::new_spanned(
            first,
            "#[fallback] first parameter must be `&self`",
        ));
    };
    if recv.mutability.is_some() {
        return Err(syn::Error::new_spanned(
            recv,
            "#[fallback] receiver must be `&self`, not `&mut self` — \
             native caps share state across threads via interior mutability behind `Arc<Self>`",
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
/// third parameter. Required signature: `(&self, ctx: &mut NativeCtx<'_>, mail: K)`.
/// The `&self` (vs `&mut self`) is load-bearing — the actor lives
/// behind `Arc<Self>` and shares the ref across dispatcher / lookup
/// consumers.
/// Extract `K` from a NativeActor handler's third parameter and a
/// flag for slice-handler shape. Accepts:
///   - `(&self, ctx: &mut NativeCtx<'_>, mail: K)` — single-payload
///     handler, decodes via `Kind::decode_from_bytes`.
///   - `(&self, ctx: &mut NativeCtx<'_>, mails: &[K])` — batched
///     cast-shape handler, decodes the whole envelope as a contiguous
///     `&[K]` slice via `decode_cast_slice` so a single envelope with
///     `count > 1` (`Mailbox::send_many`, ADR-0019) reaches the
///     handler intact. Only meaningful for cast-shape kinds; postcard
///     kinds have no batch wire.
fn extract_native_actor_handler_kind(sig: &Signature) -> syn::Result<(Type, bool)> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[actor] impl NativeActor #[handler] method must have signature \
             `(&self, ctx: &mut NativeCtx<'_>, arg: K)` (or `mail: &[K]` for batched cast kinds)",
        ));
    }
    let first = &sig.inputs[0];
    let FnArg::Receiver(recv) = first else {
        return Err(syn::Error::new_spanned(
            first,
            "#[handler] first parameter must be `&self` (NativeActor caps share state via Arc)",
        ));
    };
    if recv.mutability.is_some() {
        return Err(syn::Error::new_spanned(
            recv,
            "#[actor] impl NativeActor #[handler] receiver must be `&self`, not `&mut self` — \
             native caps share state across threads via interior mutability behind `Arc<Self>`",
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

struct NativeHandlerFn {
    method: syn::ImplItemFn,
    /// The kind's inner type. For `mail: K` this is `K`; for slice
    /// handlers `mail: &[K]` it's also `K` — the slice form just
    /// changes how the dispatcher decodes from `payload` bytes.
    kind_ty: Type,
    /// `true` for 3-arg `(&mut self, sender: ReplyTo, mail: …)` — the
    /// dispatcher forwards the envelope's `sender` through to the
    /// handler. `false` for 2-arg `(&mut self, mail: …)` — sender is
    /// ignored (fire-and-forget caps like Log).
    takes_sender: bool,
    /// `true` when the `mail` parameter is `&[K]` rather than `K`.
    /// The dispatch arm decodes the whole payload as a contiguous
    /// slice via `bytemuck::cast_slice` so a single envelope with
    /// `count > 1` (`Mailbox::send_many`, ADR-0019) reaches the
    /// handler intact. Only valid for cast-shape kinds — postcard
    /// has no batch wire (postcard slices would need length-prefix
    /// framing per element).
    is_slice: bool,
}

/// Extract `K` from a native handler's signature. Accepts two shapes:
///   - 2-arg `(&mut self, mail: K)` — fire-and-forget handler, sender
///     is ignored when the dispatcher invokes it.
///   - 3-arg `(&mut self, sender: ReplyTo, mail: K)` — reply-bearing
///     handler, sender is the envelope's reply target. The macro
///     trusts the second arg's name/type and forwards the dispatcher's
///     `sender` through to it.
///
/// Returns the kind's inner type plus the `takes_sender` and
/// `is_slice` flags the caller uses to pick the right dispatch-arm
/// shape. `is_slice = true` when the parameter is `&[K]` (batched
/// cast-shape decode); the inner `K` is what `HandlesKind` /
/// `Kind::ID` reference.
fn extract_native_handler_kind_type(sig: &Signature) -> syn::Result<(Type, bool, bool)> {
    if sig.inputs.len() != 2 && sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "native #[handler] method must have signature `(&mut self, arg: K)` \
             or `(&mut self, sender: ::aether_data::ReplyTo, arg: K)` \
             (where K may also be `&[K]` for batched cast-shape kinds)",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(
            first,
            "native #[handler] first parameter must be `&mut self`",
        ));
    }
    let takes_sender = sig.inputs.len() == 3;
    let kind_arg_idx = if takes_sender { 2 } else { 1 };
    let FnArg::Typed(pat_ty) = &sig.inputs[kind_arg_idx] else {
        return Err(syn::Error::new_spanned(
            &sig.inputs[kind_arg_idx],
            "native #[handler] kind parameter must be a typed `arg: K`",
        ));
    };
    // Detect `&[K]` slice handlers (any reference to a slice). Inner
    // `K` is what `HandlesKind` / `Kind::ID` reference; the slice
    // form just picks a different decode path in the dispatch arm.
    if let Type::Reference(type_ref) = &*pat_ty.ty
        && let Type::Slice(slice) = &*type_ref.elem
    {
        return Ok(((*slice.elem).clone(), takes_sender, true));
    }
    Ok(((*pat_ty.ty).clone(), takes_sender, false))
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
        // `Mail::kind()` returns the raw `u64` the FFI carried; `Kind::ID`
        // is typed `KindId` post-issue 466, so we drop into `.0` for the
        // comparison.
        quote! {
            if __aether_kind == <#k as ::aether_actor::__macro_internals::Kind>::ID.0 {
                if let ::core::option::Option::Some(__aether_decoded) =
                    __aether_mail.decode_kind::<#k>()
                {
                    self.#method(__aether_ctx, __aether_decoded);
                }
                return ::aether_actor::DISPATCH_HANDLED;
            }
        }
    });

    let tail = match fallback {
        Some(f) => {
            let method = &f.method.sig.ident;
            quote! {
                self.#method(__aether_ctx, __aether_mail);
                ::aether_actor::DISPATCH_HANDLED
            }
        }
        None => quote! { ::aether_actor::DISPATCH_UNKNOWN_KIND },
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
/// `aether_actor::export!()` reads these consts and emits the
/// `#[unsafe(link_section = "aether.kinds.inputs")]` static in the
/// cdylib root crate. Keeping the section emission out of this macro
/// is what prevents the section from stacking when a `#[actor]`-
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
            (1 + ::aether_actor::__macro_internals::canonical::inputs_handler_len(
                <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                #doc_expr,
            ))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_actor::__macro_internals::canonical::inputs_handler_len(
                        <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                        <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                        #doc_expr,
                    );
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_actor::__macro_internals::canonical::write_inputs_handler::<REC_LEN>(
                        <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                        <#k as ::aether_actor::__macro_internals::Kind>::NAME,
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
            (1 + ::aether_actor::__macro_internals::canonical::inputs_fallback_len(#doc_expr))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_actor::__macro_internals::canonical::inputs_fallback_len(#doc_expr);
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_actor::__macro_internals::canonical::write_inputs_fallback::<REC_LEN>(#doc_expr);
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
            (1 + ::aether_actor::__macro_internals::canonical::inputs_component_len(#doc_lit))
        });
        copy_blocks.push(quote! {
            {
                const REC_LEN: usize =
                    ::aether_actor::__macro_internals::canonical::inputs_component_len(#doc_lit);
                const REC_BYTES: [u8; REC_LEN] =
                    ::aether_actor::__macro_internals::canonical::write_inputs_component::<REC_LEN>(#doc_lit);
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
                out[#len_ident + 1] = if <#k as ::aether_actor::__macro_internals::Kind>::IS_STREAM { 1 } else { 0 };
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
