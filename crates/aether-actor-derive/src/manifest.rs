use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::Type;

use crate::handler_parse::{FallbackFn, HandlerClass, HandlerFn};

//noinspection DuplicatedCode -- proc-macro crates intentionally do not depend on one another for this leaf helper.
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
fn emit_inputs_copy_block(rec_len_expr: &TokenStream2, rec_bytes_expr: &TokenStream2) -> TokenStream2 {
    quote! {
        {
            const REC_LEN: usize = #rec_len_expr;
            const REC_BYTES: [u8; REC_LEN] = #rec_bytes_expr;
            // Per-record section version byte — a token reference to
            // `INPUTS_SECTION_VERSION` (ADR-0118 / issue 1984: the record
            // is the owned aether-wire encoding) so the writer folds from
            // the same source of truth the reader reads.
            out[pos] = ::aether_actor::__macro_internals::INPUTS_SECTION_VERSION;
            pos += 1;
            let mut i = 0;
            while i < REC_LEN {
                out[pos] = REC_BYTES[i];
                pos += 1;
                i += 1;
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn build_inputs_manifest_consts(
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
        // ADR-0112 / ADR-0134: the reply class rides the handler record as a
        // `ReplyContract` `(tag, id)` pair — `(0, 0)` for a single `-> ()`,
        // `(1, R::ID)` for a single `-> R` / `-> Pending<R>`, `(2, K::ID)` for
        // a multi handler emitting `K` (the element kind off its `Multi<K>`
        // ctx marker), `(3, 0)` for a manual handler (no single static reply
        // kind).
        let (reply_tag_expr, reply_id_expr) = match (h.class, h.reply.manifest_kind()) {
            (HandlerClass::Manual, _) => (quote! { 3u8 }, quote! { 0u64 }),
            (HandlerClass::Multi, _) => {
                let k = h.multi_kind.as_ref().expect("a multi handler carries its `Multi<K>` emit kind");
                (quote! { 2u8 }, quote! { <#k as ::aether_actor::__macro_internals::Kind>::ID.0 })
            }
            (HandlerClass::Single, Some(r)) => {
                (quote! { 1u8 }, quote! { <#r as ::aether_actor::__macro_internals::Kind>::ID.0 })
            }
            (HandlerClass::Single, None) => (quote! { 0u8 }, quote! { 0u64 }),
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
        copy_blocks.push(emit_inputs_copy_block(
            &quote! {
                ::aether_actor::__macro_internals::canonical::inputs_handler_len(
                    <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                    <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                    #doc_expr,
                    #reply_tag_expr,
                    #reply_id_expr,
                )
            },
            &quote! {
                ::aether_actor::__macro_internals::canonical::write_inputs_handler::<REC_LEN>(
                    <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                    <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                    #doc_expr,
                    #reply_tag_expr,
                    #reply_id_expr,
                )
            },
        ));
    }

    if let Some(f) = fallback {
        let doc_expr = option_str_token(f.agent_doc.as_ref());
        len_terms.push(quote! {
            (1 + ::aether_actor::__macro_internals::canonical::inputs_fallback_len(#doc_expr))
        });
        copy_blocks.push(emit_inputs_copy_block(
            &quote! {
                ::aether_actor::__macro_internals::canonical::inputs_fallback_len(#doc_expr)
            },
            &quote! {
                ::aether_actor::__macro_internals::canonical::write_inputs_fallback::<REC_LEN>(#doc_expr)
            },
        ));
    }

    if let Some(doc) = component_doc {
        let doc_lit = doc.as_str();
        len_terms.push(quote! {
            (1 + ::aether_actor::__macro_internals::canonical::inputs_component_len(#doc_lit))
        });
        copy_blocks.push(emit_inputs_copy_block(
            &quote! {
                ::aether_actor::__macro_internals::canonical::inputs_component_len(#doc_lit)
            },
            &quote! {
                ::aether_actor::__macro_internals::canonical::write_inputs_component::<REC_LEN>(#doc_lit)
            },
        ));
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
        copy_blocks.push(emit_inputs_copy_block(
            &quote! {
                ::aether_actor::__macro_internals::canonical::inputs_config_len(
                    <#cfg as ::aether_actor::__macro_internals::Kind>::ID.0,
                    <#cfg as ::aether_actor::__macro_internals::Kind>::NAME,
                )
            },
            &quote! {
                ::aether_actor::__macro_internals::canonical::write_inputs_config::<REC_LEN>(
                    <#cfg as ::aether_actor::__macro_internals::Kind>::ID.0,
                    <#cfg as ::aether_actor::__macro_internals::Kind>::NAME,
                )
            },
        ));
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
/// because the rlib archive member holding them is never extracted into
/// the link — an unextracted member loses its custom section regardless
/// of `#[used]`, which pins a symbol against dead-section stripping but
/// does not force archive extraction. Section survival comes from the
/// static living in a compilation unit that is actually linked: the
/// `aether.kinds.inputs` section survives because `#[actor]` emits it
/// here, in the consumer's own compilation unit, and we emit
/// `aether.kinds` the same way.
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
pub fn build_kinds_section_retention_statics(
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
        let schema_ident = quote::format_ident!("__AETHER_HANDLERS_KIND_SCHEMA_{}_{}", self_ty_hint, idx);
        let len_ident = quote::format_ident!("__AETHER_HANDLERS_KIND_CANONICAL_LEN_{}_{}", self_ty_hint, idx);
        let bytes_ident = quote::format_ident!("__AETHER_HANDLERS_KIND_CANONICAL_BYTES_{}_{}", self_ty_hint, idx);
        let section_ident = quote::format_ident!("__AETHER_HANDLERS_KIND_MANIFEST_{}_{}", self_ty_hint, idx);
        let labels_static_ident = quote::format_ident!("__AETHER_HANDLERS_KIND_LABELS_{}_{}", self_ty_hint, idx);
        let labels_len_ident = quote::format_ident!("__AETHER_HANDLERS_KIND_LABELS_LEN_{}_{}", self_ty_hint, idx);
        let labels_bytes_ident = quote::format_ident!("__AETHER_HANDLERS_KIND_LABELS_BYTES_{}_{}", self_ty_hint, idx);
        let labels_section_ident =
            quote::format_ident!("__AETHER_HANDLERS_KIND_LABELS_MANIFEST_{}_{}", self_ty_hint, idx);
        quote! {
            // Mirrors the intermediate-static pattern in the Kind derive
            // (`aether-data-derive`) so const-eval of the serializer
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
            // Same wire shape as the Kind derive's primary emission
            // (ADR-0118 / issue 1984: the owned aether-wire encoding) so
            // retention records (when this kind lives in a dependency
            // rlib) pair cleanly with the primary records by id.
            #[cfg(target_family = "wasm")]
            #[unsafe(link_section = "aether.kinds")]
            static #section_ident: [u8; #len_ident + 1] = {
                let mut out = [0u8; #len_ident + 1];
                out[0] = ::aether_actor::__macro_internals::KINDS_SECTION_VERSION;
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
            #[cfg(target_family = "wasm")]
            #[unsafe(link_section = "aether.kinds.labels")]
            static #labels_section_ident: [u8; #labels_len_ident + 1] = {
                let mut out = [0u8; #labels_len_ident + 1];
                // ADR-0118 / issue 1984: the owned aether-wire
                // encoding of `KindLabels`, matching the Kind derive.
                out[0] = ::aether_actor::__macro_internals::LABELS_SECTION_VERSION;
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
        return syn::Ident::new(&to_screaming_snake_case(&seg.ident.to_string()), seg.ident.span());
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
