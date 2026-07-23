// Data proc-macro codegen builds deeply-nested `quote!` trees and flat deny-list
// scans where `if let Some(..)` reads clearer than `map_or_else`. Allow at the
// crate root because cargo doesn't permit `[lints.clippy]` overrides alongside
// `lints.workspace = true` in the manifest (iamacoffeepot/aether#854 Phase 1.a).
#![allow(clippy::option_if_let_else)]

//! Proc-macro home for `aether-data`'s data-layer macros:
//! `#[derive(Kind)]`, `#[derive(Schema)]`, and `#[transform]`.
//!
//! `Kind` and `Schema` are per ADR-0019 / ADR-0031 / ADR-0032. This
//! crate is kept separate from `aether-data` because Rust requires
//! proc-macro crates to opt into `proc-macro = true` and forbids them
//! from exporting non-macro items; pairing them in the same crate would
//! force every consumer through the proc-macro toolchain even when they
//! just want the runtime traits.
//!
//! `Kind` emits the `aether_data::Kind` impl (`const NAME`, `const ID`,
//! optional `const IS_INPUT`) plus the `#[link_section]` statics for
//! both `aether.kinds` (canonical schema bytes) and
//! `aether.kinds.labels` (nominal sidecar). The ID is
//! `fnv1a_64_prefixed(KIND_DOMAIN, canonical_bytes_of(name, schema))`,
//! matching the substrate-side derivation byte-for-byte (ADR-0030
//! Phase 2 / ADR-0032). The `KIND_DOMAIN` prefix disjoins the
//! `Kind::ID` space from `MailboxId` (issue #186). Consumers must also
//! derive (or hand-roll) `Schema` on the type — the Kind derive walks
//! `<Self as Schema>::SCHEMA` for canonical bytes and
//! `<Self as Schema>::LABEL_NODE` for the labels tree.
//!
//! `Schema` emits three consts per impl: `SCHEMA` (the `SchemaType`
//! tree, const-constructible per ADR-0031), `LABEL` (the
//! `Option<&'static str>` Rust type path from `module_path!()`), and
//! `LABEL_NODE` (the parallel-shape labels tree the kind's sidecar
//! record embeds). It also emits `CastEligible` so `repr_c` flags
//! propagate — field types used as cast-shaped payloads get
//! eligibility for free without a second derive.
//!
//! Field-type handling delegates to `<FieldT as Schema>::SCHEMA` /
//! `LABEL_NODE` for all cross-crate resolution. The one exception is
//! `Vec<u8>` — stable Rust forbids the specialization (`Vec<u8>` would
//! overlap `Vec<T>` because `u8: Schema`), so the derive pattern-matches
//! the field type's syntax and emits `SchemaType::Bytes` /
//! `LabelNode::Anonymous` directly when it sees `Vec<u8>`. Every other
//! shape goes through trait dispatch.
//!
//! A transform is a **data-layer primitive** — a pure `Kind -> Kind`
//! function with zero dependence on the actor framework. Its runtime
//! types (`TransformEntry`, `TransformError`, the link-time inventory)
//! live in `aether-data`; this crate is the sibling proc-macro that
//! `aether-data` cannot itself be (`proc-macro = true` forbids
//! exporting runtime items). `aether-data` re-exports the macro as
//! `aether_data::transform` behind the `derive` feature.
//!
//! The macro's three ADR-0048 §1 responsibilities:
//!
//! 1. **Stable name-based `transform_id`.**
//!    `fnv1a_64(TRANSFORM_DOMAIN ++ "{crate}::{module_path}::{fn}")`,
//!    tagged `Tag::Transform`. Built at the *consumer's* compile time
//!    from `concat!(env!("CARGO_PKG_NAME"), "::", module_path!(), "::",
//!    fn)` so identity tracks the fully-qualified name, not the
//!    position in the file.
//! 2. **Deny-list purity scan.** Walks the body's expression paths and
//!    rejects host-fn imports, handler-context types, the sync
//!    request/reply primitive, and compile-time-catchable
//!    nondeterminism sources (`std::env`, `std::time`, `core::time`).
//!    Best-effort: it sees only the immediate body, not helper-fn
//!    bodies, and there is no runtime sandbox (ADR-0048
//!    Consequences/Negative). First-party review is the other defense.
//! 3. **Link-time inventory submission.** Emits an `inventory::submit!`
//!    of a `TransformEntry` carrying the id, input/output kind ids, the
//!    name, and a type-erased `invoke` thunk that decodes each input
//!    slice, calls the user fn, and encodes the output.
//!
//! There is no FFI shim, no `extern "C"`, no custom section — the
//! original wasm-export design was deferred (ADR-0048 revision
//! 2026-05-20).

use core::iter;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, Fields, FnArg, GenericArgument, ItemFn, Lit, Meta,
    PathArguments, ReturnType, Type, parse_macro_input,
};

/// ADR-0048 §1 cap on input parameters.
const MAX_TRANSFORM_INPUTS: usize = 8;

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
        return Err(syn::Error::new_spanned(u.union_token, "Kind derive does not support unions"));
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
    let schema_static_ident = format_ident!("__AETHER_SCHEMA_{}", upper);
    let canonical_len_ident = format_ident!("__AETHER_CANONICAL_LEN_{}", upper);
    let canonical_bytes_ident = format_ident!("__AETHER_CANONICAL_BYTES_{}", upper);
    let labels_ident = format_ident!("__AETHER_KIND_LABELS_{}", upper);
    let labels_len_ident = format_ident!("__AETHER_LABELS_LEN_{}", upper);
    let labels_bytes_ident = format_ident!("__AETHER_LABELS_BYTES_{}", upper);
    let kind_static_ident = format_ident!("__AETHER_KIND_MANIFEST_{}", upper);
    let kind_labels_static_ident = format_ident!("__AETHER_KIND_LABELS_MANIFEST_{}", upper);

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

        // ADR-0028 / ADR-0032 / ADR-0118: `aether.kinds` ships
        // `[version_byte][canonical_bytes]`, where the canonical bytes are
        // now the owned aether-wire encoding (issue 1984) — so every
        // `Kind::ID` regenerates, gated loudly behind this version byte.
        // The version byte is `aether_data::KINDS_SECTION_VERSION`, the
        // single source of truth the reader also reads.
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = "aether.kinds")]
        static #kind_static_ident: [u8; #canonical_len_ident + 1] = {
            let mut out = [0u8; #canonical_len_ident + 1];
            out[0] = ::aether_data::KINDS_SECTION_VERSION;
            let mut i = 0;
            while i < #canonical_len_ident {
                out[i + 1] = #canonical_bytes_ident[i];
                i += 1;
            }
            out
        };

        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = "aether.kinds.labels")]
        static #kind_labels_static_ident: [u8; #labels_len_ident + 1] = {
            let mut out = [0u8; #labels_len_ident + 1];
            // ADR-0118 / issue 1984: the labels record is the owned
            // aether-wire encoding of `KindLabels`. v0x03 made records
            // self-identifying (`kind_id`); the reader still pairs by id.
            // The version byte is `aether_data::LABELS_SECTION_VERSION`,
            // the single source of truth the reader also reads.
            out[0] = ::aether_data::LABELS_SECTION_VERSION;
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
        #[cfg(not(target_family = "wasm"))]
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
        Data::Enum(e) => (expand_schema_enum(e)?, expand_label_node_enum(&name_str, e), quote! { false }),
        Data::Union(u) => {
            return Err(syn::Error::new_spanned(u.union_token, "Schema derive does not support unions"));
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
                    let fname = f.ident.as_ref().map(ToString::to_string).unwrap_or_default();
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
            // has something to render in `describe_kinds`. The wire
            // format doesn't care; field names are advisory metadata.
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
                let field_exprs = unnamed.unnamed.iter().map(|f| field_type_schema_expr(&f.ty));
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

/// Returns `true` when `ty` is syntactically a `Vec<u8>` (or any qualified
/// spelling whose outer type ends in `Vec` and whose element type's last
/// segment is `u8`, e.g. `Vec<core::primitive::u8>`). The check is purely
/// syntactic — a type alias (`type Blob = Vec<u8>`) is not resolved by the
/// proc macro and falls through to the generic `Vec` schema.
fn is_vec_u8(ty: &Type) -> bool {
    let Type::Path(tp) = ty else {
        return false;
    };
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
    // Match on the last segment so qualified spellings like
    // `core::primitive::u8` are recognized alongside bare `u8`.
    inner.path.segments.last().is_some_and(|seg| seg.ident == "u8")
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
/// implemented" error pointing at the inner `HashMap`. The walk is
/// total: it also recurses through array, slice, reference, tuple,
/// paren, and group positions so a `HashMap` hidden inside `[_; N]`,
/// `&[_]`, or `(_, _)` earns the same pointed error rather than the
/// opaque downstream one. A set type (`HashSet`/`BTreeSet`) has no
/// `Schema` impl at all, so it is rejected here too with a redirect to
/// a sorted `Vec<T>`.
fn reject_hashmap(ty: &Type) -> syn::Result<()> {
    match ty {
        Type::Path(tp) => {
            if let Some(seg) = tp.path.segments.last() {
                if seg.ident == "HashMap" {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "HashMap is not allowed in derived kind schemas — its iteration order is \
                         platform-dependent and would diverge canonical schema bytes (and Kind::ID) \
                         across builds. Use `std::collections::BTreeMap` instead, which sorts by key. \
                         See https://github.com/iamacoffeepot/aether/issues/232",
                    ));
                }
                if seg.ident == "HashSet" || seg.ident == "BTreeSet" {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "a set has no `Schema` impl and is not allowed in derived kind schemas — \
                         model it as a sorted `Vec<T>`, which encodes deterministically. \
                         See https://github.com/iamacoffeepot/aether/issues/232",
                    ));
                }
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
        Type::Array(a) => reject_hashmap(&a.elem)?,
        Type::Slice(s) => reject_hashmap(&s.elem)?,
        Type::Reference(r) => reject_hashmap(&r.elem)?,
        Type::Paren(p) => reject_hashmap(&p.elem)?,
        Type::Group(g) => reject_hashmap(&g.elem)?,
        Type::Tuple(t) => {
            for elem in &t.elems {
                reject_hashmap(elem)?;
            }
        }
        _ => {}
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
        attrs.first().map_or_else(proc_macro2::Span::call_site, Spanned::span),
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
        Fields::Named(named) => {
            named.named.iter().map(|f| FieldInfo { ident: f.ident.clone(), ty: f.ty.clone() }).collect()
        }
        Fields::Unnamed(unnamed) => {
            unnamed.unnamed.iter().map(|f| FieldInfo { ident: None, ty: f.ty.clone() }).collect()
        }
        Fields::Unit => Vec::new(),
    })
}

struct FieldInfo {
    ident: Option<syn::Ident>,
    ty: Type,
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

/// `#[transform]` — register a pure `Kind -> Kind` function as a native
/// transform (ADR-0048 §1). The annotated fn is left intact (so it
/// stays unit-testable as an ordinary fn) and gains a link-time
/// inventory entry the substrate's `TransformRegistry` collects at
/// startup.
///
/// See the crate docs for the three responsibilities (id derivation,
/// purity scan, inventory submission). The macro emits `compile_error!`
/// for a non-fn item, a `self` receiver, generics, a 9th input
/// parameter, a missing return type, or a body that names a denied
/// path.
#[proc_macro_attribute]
pub fn transform(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match expand_transform(&func) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_transform(func: &ItemFn) -> syn::Result<TokenStream2> {
    let (input_types, output_type) = validate_signature(func)?;

    // Deny-list purity scan over the immediate body (ADR-0048 §1). Best
    // effort — helper-fn bodies aren't visible here.
    purity_scan(&func.block)?;

    let inventory = emit_inventory(func, &input_types, output_type);
    Ok(quote! {
        #func
        #inventory
    })
}

/// Enforce the ADR-0048 §1 signature contract: no generics, no `self`,
/// ≤ 8 inputs, and a single (non-`()`) return type. Returns the input
/// parameter types in slot order plus the output type.
fn validate_signature(func: &ItemFn) -> syn::Result<(Vec<&Type>, &Type)> {
    let sig = &func.sig;

    // No generics — transforms are monomorphic (ADR-0048 §1).
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new(
            sig.generics.span(),
            "transforms cannot be generic -- they are monomorphic Kind -> Kind functions \
             (ADR-0048 §1)",
        ));
    }

    // Collect the input parameter types, rejecting any `self` receiver
    // and capping at 8 (ADR-0048 §1).
    let mut input_types: Vec<&Type> = Vec::new();
    for arg in &sig.inputs {
        match arg {
            FnArg::Receiver(recv) => {
                return Err(syn::Error::new(
                    recv.span(),
                    "transforms cannot take `self` -- they are free-standing pure functions \
                     (ADR-0048 §1)",
                ));
            }
            FnArg::Typed(pat) => input_types.push(&pat.ty),
        }
    }
    if input_types.len() > MAX_TRANSFORM_INPUTS {
        return Err(syn::Error::new(
            sig.inputs.span(),
            format!(
                "transforms accept at most {MAX_TRANSFORM_INPUTS} inputs (ADR-0048 §1); found {}",
                input_types.len(),
            ),
        ));
    }

    // Single return type, also a `Kind` (ADR-0048 §1). A bare `()`
    // return is rejected — a transform produces a kind value.
    let output_type: &Type = match &sig.output {
        ReturnType::Type(_, ty) => ty,
        ReturnType::Default => {
            return Err(syn::Error::new(sig.span(), "transforms must return a single Kind value (ADR-0048 §1)"));
        }
    };

    Ok((input_types, output_type))
}

/// Emit the per-type `Kind` bound assertions + the link-time inventory
/// submission (id derivation, the static input-kind-id slice, and the
/// type-erased `invoke` thunk). Codegen only — the signature is already
/// validated and the body already purity-scanned.
fn emit_inventory(func: &ItemFn, input_types: &[&Type], output_type: &Type) -> TokenStream2 {
    let fn_name = &func.sig.ident;
    let fn_name_str = fn_name.to_string();

    // Per-type `Kind` bound assertions: the macro can't check trait
    // bounds at expansion time, so emit a `const _: fn() = || { <T as
    // Kind>::ID; };` per input + output. A build error fires if any type
    // doesn't impl `Kind` (ADR-0048 §1).
    let bound_assertions = input_types.iter().chain(iter::once(&output_type)).map(|ty| {
        quote! {
            const _: fn() = || {
                let _ = <#ty as ::aether_data::transform::__transform_runtime::Kind>::ID;
            };
        }
    });

    // Fully-qualified name string at the consumer's compile time:
    // `"{crate}::{module_path}::{fn}"`. `module_path!()` already begins
    // with the crate's lib/bin name as the first segment, so prefixing
    // `CARGO_PKG_NAME` keeps the id stable even when two crates share a
    // module path tail.
    let name_expr = quote! {
        ::core::concat!(
            ::core::env!("CARGO_PKG_NAME"), "::",
            ::core::module_path!(), "::",
            #fn_name_str,
        )
    };

    // The `invoke` thunk: decode each input slice (slot-index order)
    // against its declared kind via `Kind::decode_from_bytes`, call the
    // user fn, encode the output via `Kind::encode_into_bytes`. Decode
    // failure -> `InputDecode { slot }`; arity mismatch ->
    // `InputArity`. The output-byte cap is the executor's job, not the
    // thunk's.
    let arity = input_types.len();
    let decode_bindings = input_types.iter().enumerate().map(|(slot, ty)| {
        let local = format_ident!("__in{slot}");
        quote! {
            let #local: #ty = match
                <#ty as ::aether_data::transform::__transform_runtime::Kind>::decode_from_bytes(
                    __inputs[#slot],
                )
            {
                ::core::option::Option::Some(v) => v,
                ::core::option::Option::None => {
                    return ::core::result::Result::Err(
                        ::aether_data::transform::__transform_runtime::TransformError::InputDecode {
                            slot: #slot,
                        },
                    );
                }
            };
        }
    });
    let decode_locals = (0..arity).map(|slot| format_ident!("__in{slot}"));

    let entry_static = format_ident!("__AETHER_TRANSFORM_ENTRY_{}", fn_name_str.to_uppercase());

    // Static slices the inventory entry borrows. `inventory::submit!`
    // needs const-constructible borrows, so the input-kind-id list is a
    // file-scoped `static` array rather than an inline literal.
    let input_kinds_static = format_ident!("__AETHER_TRANSFORM_INPUTS_{}", fn_name_str.to_uppercase());
    let input_kind_exprs = input_types.iter().map(|ty| {
        quote! {
            <#ty as ::aether_data::transform::__transform_runtime::Kind>::ID
        }
    });

    quote! {
        #(#bound_assertions)*

        // Link-time inventory submission (ADR-0048 §1). Cfg-gated to
        // non-wasm targets because `inventory` doesn't link on
        // `wasm32-unknown-unknown` (same gate as the Kind derive's
        // descriptor inventory).
        #[cfg(not(target_arch = "wasm32"))]
        const _: () = {
            static #input_kinds_static:
                [::aether_data::transform::__transform_runtime::KindId; #arity] = [
                    #(#input_kind_exprs),*
                ];

            fn #entry_static(__inputs: &[&[u8]])
                -> ::core::result::Result<
                    ::aether_data::transform::__transform_runtime::Vec<u8>,
                    ::aether_data::transform::__transform_runtime::TransformError,
                >
            {
                if __inputs.len() != #arity {
                    return ::core::result::Result::Err(
                        ::aether_data::transform::__transform_runtime::TransformError::InputArity {
                            expected: #arity,
                            actual: __inputs.len(),
                        },
                    );
                }
                #(#decode_bindings)*
                let __out: #output_type = #fn_name(#(#decode_locals),*);
                ::core::result::Result::Ok(
                    <#output_type as ::aether_data::transform::__transform_runtime::Kind>::encode_into_bytes(
                        &__out,
                    ),
                )
            }

            ::aether_data::transform::__transform_runtime::inventory::submit! {
                ::aether_data::transform::__transform_runtime::TransformEntry {
                    transform_id:
                        ::aether_data::transform::__transform_runtime::TransformId(
                            ::aether_data::with_tag(
                                ::aether_data::Tag::Transform,
                                ::aether_data::fnv1a_64_prefixed(
                                    &::aether_data::TRANSFORM_DOMAIN,
                                    #name_expr.as_bytes(),
                                ),
                            ),
                        ),
                    input_kind_ids: &#input_kinds_static,
                    output_kind_id:
                        <#output_type as ::aether_data::transform::__transform_runtime::Kind>::ID,
                    name: #name_expr,
                    invoke: #entry_static
                        as ::aether_data::transform::__transform_runtime::InvokeFn,
                }
            }
        };
    }
}

/// Walk a function body's expression paths and reject any that name a
/// denied path (ADR-0048 §1 deny-list). Returns the first violation as
/// a span-located `compile_error!`.
fn purity_scan(block: &syn::Block) -> syn::Result<()> {
    let mut scanner = PurityScanner { violation: None };
    scanner.visit_block(block);
    match scanner.violation {
        Some(span) => Err(syn::Error::new(
            span,
            "transforms cannot call host functions or access handler context -- see ADR-0048",
        )),
        None => Ok(()),
    }
}

/// One denied path: a sequence of `::`-joined segment tails. A body path
/// matches if its trailing segments end with this sequence (so both
/// `aether::send_mail_p32` and a `use`-shortened `send_mail_p32` are
/// caught for the single-segment entries, and qualified `std::time::*`
/// is caught by the two-segment prefix entries).
struct DeniedPath {
    /// Segments to match against the *trailing* run of a body path.
    tail: &'static [&'static str],
}

/// The deny-list (ADR-0048 §1):
/// - host-fn imports (`aether::send_mail_p32`, `reply_mail_p32`,
///   `resolve_*_p32`, and the other SDK host fns),
/// - handler-context types (`aether_actor::Ctx`, `OutboundReply`),
/// - compile-time-catchable nondeterminism (`std::env::*`,
///   `std::time::*`, `core::time::*`).
const DENY_LIST: &[DeniedPath] = &[
    // Host fns — match the bare fn tail so both qualified and
    // use-shortened call sites are caught.
    DeniedPath { tail: &["send_mail_p32"] },
    DeniedPath { tail: &["reply_mail_p32"] },
    DeniedPath { tail: &["send_mail_traced_p32"] },
    DeniedPath { tail: &["save_state_p32"] },
    DeniedPath { tail: &["resolve_mailbox_p32"] },
    DeniedPath { tail: &["resolve_kind_p32"] },
    // Handler-context types.
    DeniedPath { tail: &["aether_actor", "Ctx"] },
    DeniedPath { tail: &["aether_actor", "OutboundReply"] },
    // Nondeterminism sources, by two-segment prefix so any item under
    // them (`now`, `Instant`, `var`, etc.) is rejected.
    DeniedPath { tail: &["std", "env"] },
    DeniedPath { tail: &["std", "time"] },
    DeniedPath { tail: &["core", "time"] },
];

/// Body-path collector + matcher. Records the span of the first path
/// whose trailing segments match a deny-list entry.
struct PurityScanner {
    violation: Option<proc_macro2::Span>,
}

impl PurityScanner {
    /// Check one path's segment idents against the deny-list. A
    /// deny-entry matches if the path's trailing segments equal the
    /// entry's `tail` sequence (so `std::time::Instant::now` matches the
    /// `["std", "time"]` entry, and a use-shortened `send_mail_p32`
    /// matches the single-segment `["send_mail_p32"]` entry).
    fn check_path(&mut self, path: &syn::Path) {
        if self.violation.is_some() {
            return;
        }
        let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        for denied in DENY_LIST {
            // Single-segment fn entries match the fn ident anywhere in
            // the path (catches both `aether::send_mail_p32` and a
            // `use`-shortened `send_mail_p32`). Multi-segment prefix
            // entries (the `std::time` / `core::time` / `std::env`
            // nondeterminism roots, plus the `aether_actor::Ctx` types)
            // anchor at the path head so any item beneath them is
            // rejected.
            let matched = if denied.tail.len() == 1 {
                segs.iter().any(|s| s == denied.tail[0])
            } else {
                segs.len() >= denied.tail.len() && segs[..denied.tail.len()] == *denied.tail
            };
            if matched {
                self.violation = Some(path.span());
                return;
            }
        }
    }
}

impl<'ast> Visit<'ast> for PurityScanner {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        // A call's callee is an `Expr::Path` for free-fn / path calls;
        // `visit_expr_path` handles it. Method calls (`x.foo()`) carry
        // no callee path, so a `std::time::Instant::now()` written as a
        // path-call is the case that matters here — already covered.
        if let Expr::Path(p) = &*node.func {
            self.check_path(&p.path);
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        self.check_path(&node.path);
        visit::visit_expr_path(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::reject_hashmap;
    use syn::parse_str;

    // Issue #232: pin the HashMap rejection so a future field-walker
    // refactor can't silently drop the check. Each fixture covers one
    // shape we expect the rejection to catch.
    fn err(ty: &str) -> String {
        let parsed: syn::Type = parse_str(ty).expect("test fixture parses");
        reject_hashmap(&parsed).err().unwrap_or_else(|| panic!("expected reject_hashmap to error on {ty}")).to_string()
    }

    #[test]
    fn rejects_direct_hashmap_field() {
        let msg = err("HashMap<String, String>");
        assert!(msg.contains("BTreeMap"), "error must point to BTreeMap fix, got: {msg}");
        assert!(msg.contains("232"), "error must reference issue 232, got: {msg}");
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
    fn rejects_hashmap_in_array() {
        let msg = err("[HashMap<String, u32>; 4]");
        assert!(msg.contains("BTreeMap"));
        assert!(msg.contains("232"));
    }

    #[test]
    fn rejects_hashmap_in_tuple() {
        let msg = err("(u32, HashMap<String, u32>)");
        assert!(msg.contains("BTreeMap"));
        assert!(msg.contains("232"));
    }

    #[test]
    fn rejects_hashmap_behind_slice_ref() {
        let msg = err("&[HashMap<String, u32>]");
        assert!(msg.contains("BTreeMap"));
        assert!(msg.contains("232"));
    }

    #[test]
    fn rejects_hashmap_behind_ref() {
        let msg = err("&HashMap<String, u32>");
        assert!(msg.contains("BTreeMap"));
        assert!(msg.contains("232"));
    }

    #[test]
    fn rejects_hashset() {
        let msg = err("HashSet<u64>");
        assert!(msg.contains("Vec"), "error must redirect to a sorted Vec, got: {msg}");
    }

    #[test]
    fn rejects_btreeset() {
        let msg = err("BTreeSet<u64>");
        assert!(msg.contains("Vec"), "error must redirect to a sorted Vec, got: {msg}");
    }

    #[test]
    fn allows_btreemap_field() {
        let parsed: syn::Type = parse_str("BTreeMap<String, String>").expect("test setup: BTreeMap type parses");
        assert!(reject_hashmap(&parsed).is_ok());
    }

    #[test]
    fn allows_plain_types() {
        for ty in ["u32", "String", "Vec<u8>", "Option<String>"] {
            let parsed: syn::Type = parse_str(ty).expect("test setup: candidate type parses as syn::Type");
            assert!(reject_hashmap(&parsed).is_ok(), "rejected {ty} unexpectedly");
        }
    }
}
