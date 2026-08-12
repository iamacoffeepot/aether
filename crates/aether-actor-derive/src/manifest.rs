use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::Type;

use crate::handler_parse::{FallbackFn, HandlerClass, HandlerFn};
use crate::opts::{ActorCardinality, ActorOpts};

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

/// Emit one version-framed manifest record into the surrounding const writer.
/// The caller supplies the section version and the aether-wire record body,
/// so inputs and actor-lineage manifests share one framing protocol.
fn emit_record_copy_block(
    section_version_expr: &TokenStream2,
    record_len_expr: &TokenStream2,
    record_bytes_expr: &TokenStream2,
) -> TokenStream2 {
    quote! {
        {
            const RECORD_LEN: usize = #record_len_expr;
            const RECORD_BYTES: [u8; RECORD_LEN] = #record_bytes_expr;
            out[pos] = #section_version_expr;
            pos += 1;
            let mut index = 0;
            while index < RECORD_LEN {
                out[pos] = RECORD_BYTES[index];
                pos += 1;
                index += 1;
            }
        }
    }
}

/// One record's contribution to a manifest: the statement that adds its byte
/// length to the running total, and the block that copies its bytes into the
/// output array. Both are statements so a `#[cfg]` can gate them — a `#[cfg]`
/// cannot gate an operand of `+`, which is why the length is accumulated rather
/// than folded (iamacoffeepot/aether#4811).
struct RecordTerms {
    len_stmt: TokenStream2,
    copy_block: TokenStream2,
}

/// Wrap a raw length expression and copy block in the statement forms
/// [`RecordTerms`] carries, gated by `cfgs`.
fn record_terms(cfgs: &[syn::Attribute], len_expr: &TokenStream2, copy_block: TokenStream2) -> RecordTerms {
    RecordTerms {
        len_stmt: quote! {
            #(#cfgs)*
            {
                len += #len_expr;
            }
        },
        copy_block: quote! {
            #(#cfgs)*
            #copy_block
        },
    }
}

/// The manifest terms for one handler's `aether.kinds.inputs` record. Shared by
/// the actor-side manifest and the ADR-0169 handler-set manifest so a set's
/// records are byte-identical to the ones the same handler would have produced
/// declared locally.
fn handler_record_terms(h: &HandlerFn, section_version: &TokenStream2) -> RecordTerms {
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
    // `inputs_handler_len` / `write_inputs_handler` take a raw `u64` for the
    // wire bytes; `Kind::ID` is `KindId` post-issue 466 so we drop into `.0`.
    let record_len = quote! {
        ::aether_actor::__macro_internals::canonical::inputs_handler_len(
            <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
            <#k as ::aether_actor::__macro_internals::Kind>::NAME,
            #doc_expr,
            #reply_tag_expr,
            #reply_id_expr,
        )
    };
    let copy_block = emit_record_copy_block(
        section_version,
        &record_len,
        &quote! {
            ::aether_actor::__macro_internals::canonical::write_inputs_handler::<RECORD_LEN>(
                <#k as ::aether_actor::__macro_internals::Kind>::ID.0,
                <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                #doc_expr,
                #reply_tag_expr,
                #reply_id_expr,
            )
        },
    );
    record_terms(&h.cfgs, &quote! { (1 + #record_len) }, copy_block)
}

/// ADR-0169: the `&'static [u8]` manifest bytes for a handler set's own
/// records. A slice rather than the actor-side `[u8; LEN]` pair because a
/// trait cannot carry an associated const whose *type* mentions `Self::LEN`;
/// `<[u8]>::len` is const, so an adopter still recovers the length for its own
/// array arithmetic.
pub fn build_handler_set_manifest_const(handlers: &[HandlerFn]) -> TokenStream2 {
    let section_version = quote! {
        ::aether_actor::__macro_internals::INPUTS_SECTION_VERSION
    };
    let (len_stmts, copy_blocks): (Vec<_>, Vec<_>) = handlers
        .iter()
        .map(|h| {
            let terms = handler_record_terms(h, &section_version);
            (terms.len_stmt, terms.copy_block)
        })
        .unzip();
    quote! {
        &{
            const LEN: usize = {
                let mut len = 0usize;
                #(#len_stmts)*
                len
            };
            let mut out = [0u8; LEN];
            let mut pos: usize = 0usize;
            #(#copy_blocks)*
            let _ = pos;
            out
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn build_inputs_manifest_consts(
    handlers: &[HandlerFn],
    fallback: Option<&FallbackFn>,
    component_doc: Option<&String>,
    config_kind_ty: Option<&Type>,
    handler_set: Option<(&syn::Path, &Type)>,
) -> TokenStream2 {
    let mut len_stmts: Vec<TokenStream2> = Vec::new();
    let mut copy_blocks: Vec<TokenStream2> = Vec::new();
    let section_version = quote! {
        ::aether_actor::__macro_internals::INPUTS_SECTION_VERSION
    };

    for h in handlers {
        let terms = handler_record_terms(h, &section_version);
        len_stmts.push(terms.len_stmt);
        copy_blocks.push(terms.copy_block);
    }

    // The fallback, component-doc, and config records belong to the actor rather
    // than to any one handler, so they carry no `#[cfg]` of their own.
    let mut push_ungated = |len_expr: &TokenStream2, copy_block: TokenStream2| {
        let terms = record_terms(&[], len_expr, copy_block);
        len_stmts.push(terms.len_stmt);
        copy_blocks.push(terms.copy_block);
    };

    if let Some(f) = fallback {
        let doc_expr = option_str_token(f.agent_doc.as_ref());
        let record_len = quote! {
            ::aether_actor::__macro_internals::canonical::inputs_fallback_len(#doc_expr)
        };
        push_ungated(
            &quote! { (1 + #record_len) },
            emit_record_copy_block(
                &section_version,
                &record_len,
                &quote! {
                    ::aether_actor::__macro_internals::canonical::write_inputs_fallback::<RECORD_LEN>(#doc_expr)
                },
            ),
        );
    }

    if let Some(doc) = component_doc {
        let doc_lit = doc.as_str();
        let record_len = quote! {
            ::aether_actor::__macro_internals::canonical::inputs_component_len(#doc_lit)
        };
        push_ungated(
            &quote! { (1 + #record_len) },
            emit_record_copy_block(
                &section_version,
                &record_len,
                &quote! {
                    ::aether_actor::__macro_internals::canonical::write_inputs_component::<RECORD_LEN>(#doc_lit)
                },
            ),
        );
    }

    // ADR-0090 (issue 1257): emit a `Config` record keyed by the
    // declared config kind's `Kind::ID` / `NAME`. Only present when the
    // user spelled `type Config = …` — `config_kind_ty` is `None` for
    // the macro-synthesized `()` case, so a no-config component stays
    // clean. Variant tag `0x03` matches `InputsRecord::Config`.
    if let Some(cfg) = config_kind_ty {
        let record_len = quote! {
            ::aether_actor::__macro_internals::canonical::inputs_config_len(
                <#cfg as ::aether_actor::__macro_internals::Kind>::ID.0,
                <#cfg as ::aether_actor::__macro_internals::Kind>::NAME,
            )
        };
        push_ungated(
            &quote! { (1 + #record_len) },
            emit_record_copy_block(
                &section_version,
                &record_len,
                &quote! {
                    ::aether_actor::__macro_internals::canonical::write_inputs_config::<RECORD_LEN>(
                        <#cfg as ::aether_actor::__macro_internals::Kind>::ID.0,
                        <#cfg as ::aether_actor::__macro_internals::Kind>::NAME,
                    )
                },
            ),
        );
    }

    // ADR-0169: an adopted handler set's records join this actor's manifest, so
    // `describe_component` reports the full receive surface and input-stream
    // subscription (derived from the manifest post-register, issue #403) covers
    // inherited kinds without separate plumbing. The set's bytes are already
    // version-framed per record, so they copy in wholesale.
    // The consts below are nested items, where `Self` does not resolve, so the
    // adopting type is named concretely.
    if let Some((set, self_ty)) = handler_set {
        push_ungated(
            &quote! { <#self_ty as #set>::__AETHER_HANDLER_SET_MANIFEST.len() },
            quote! {
                {
                    const SET_BYTES: &'static [u8] = <#self_ty as #set>::__AETHER_HANDLER_SET_MANIFEST;
                    let mut index = 0;
                    while index < SET_BYTES.len() {
                        out[pos] = SET_BYTES[index];
                        pos += 1;
                        index += 1;
                    }
                }
            },
        );
    }

    quote! {
        #[doc(hidden)]
        // The accumulator starts at zero, so a manifest with no surviving record
        // — every handler `#[cfg]`-stripped — is still well-typed.
        pub const __AETHER_INPUTS_MANIFEST_LEN: usize = {
            let mut len = 0usize;
            #(#len_stmts)*
            len
        };

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

/// Emit hidden per-actor lineage bytes as associated consts. As with the
/// inputs manifest, `#[actor]` owns only const data; `export!` is the sole
/// custom-section retention point so metadata from transitive wasm rlibs
/// cannot stack in the final module.
#[allow(clippy::too_many_lines)] // one walk keeps wire records and runtime placement facts on the same option source
pub fn build_actor_lineage_manifest_consts(self_ty: &Type, opts: &ActorOpts) -> TokenStream2 {
    let actor_namespace = quote! { <#self_ty as ::aether_actor::Addressable>::NAMESPACE };
    let actor_tag = quote! {
        ::aether_actor::__macro_internals::ActorId::singleton(#actor_namespace).0
    };
    let mut len_terms = Vec::new();
    let mut copy_blocks = Vec::new();
    let section_version = quote! {
        ::aether_actor::__macro_internals::ACTOR_LINEAGE_SECTION_VERSION
    };

    // A root record is an address anchor, so mirror native `RootEntry`:
    // an instanced namespace cannot identify the one actor an anchor needs.
    let anchors = opts.root && !matches!(opts.cardinality, Some(ActorCardinality::Instanced));
    if anchors {
        len_terms.push(quote! {
            1 + ::aether_actor::__macro_internals::actor_lineage_root_len(
                #actor_tag,
                #actor_namespace,
            )
        });
        copy_blocks.push(emit_record_copy_block(
            &section_version,
            &quote! {
                ::aether_actor::__macro_internals::actor_lineage_root_len(
                    #actor_tag,
                    #actor_namespace,
                )
            },
            &quote! {
                ::aether_actor::__macro_internals::write_actor_lineage_root::<RECORD_LEN>(
                    #actor_tag,
                    #actor_namespace,
                )
            },
        ));
    }

    for parent in &opts.child_of {
        let parent_namespace = quote! { <#parent as ::aether_actor::Addressable>::NAMESPACE };
        let parent_tag = quote! {
            ::aether_actor::__macro_internals::ActorId::singleton(#parent_namespace).0
        };
        len_terms.push(quote! {
            1 + ::aether_actor::__macro_internals::actor_lineage_child_len(
                #parent_tag,
                #actor_tag,
                #parent_namespace,
                #actor_namespace,
            )
        });
        copy_blocks.push(emit_record_copy_block(
            &section_version,
            &quote! {
                ::aether_actor::__macro_internals::actor_lineage_child_len(
                    #parent_tag,
                    #actor_tag,
                    #parent_namespace,
                    #actor_namespace,
                )
            },
            &quote! {
                ::aether_actor::__macro_internals::write_actor_lineage_child::<RECORD_LEN>(
                    #parent_tag,
                    #actor_tag,
                    #parent_namespace,
                    #actor_namespace,
                )
            },
        ));
    }

    if opts.composable {
        len_terms.push(quote! {
            1 + ::aether_actor::__macro_internals::actor_lineage_module_child_len(
                #actor_tag,
                #actor_namespace,
            )
        });
        copy_blocks.push(emit_record_copy_block(
            &section_version,
            &quote! {
                ::aether_actor::__macro_internals::actor_lineage_module_child_len(
                    #actor_tag,
                    #actor_namespace,
                )
            },
            &quote! {
                ::aether_actor::__macro_internals::write_actor_lineage_module_child::<RECORD_LEN>(
                    #actor_tag,
                    #actor_namespace,
                )
            },
        ));
    }

    let len_expr = if len_terms.is_empty() {
        quote! { 0usize }
    } else {
        quote! { #(#len_terms)+* }
    };
    let is_instanced = matches!(opts.cardinality, Some(ActorCardinality::Instanced));
    let module_child = opts.composable;
    let exact_parent_tags = opts.child_of.iter().map(|parent| {
        quote! {
            ::aether_actor::__macro_internals::ActorTypeTag::of::<#parent>()
        }
    });

    quote! {
        #[doc(hidden)]
        pub const __AETHER_PLACEMENT: ::aether_actor::__macro_internals::WasmPlacementFacts =
            ::aether_actor::__macro_internals::WasmPlacementFacts {
                is_instanced: #is_instanced,
                module_child: #module_child,
                exact_parent_tags: &[#(#exact_parent_tags),*],
            };

        #[doc(hidden)]
        pub const __AETHER_LINEAGE_MANIFEST_LEN: usize = #len_expr;

        #[doc(hidden)]
        pub const __AETHER_LINEAGE_MANIFEST: [u8; Self::__AETHER_LINEAGE_MANIFEST_LEN] = {
            let mut out = [0u8; Self::__AETHER_LINEAGE_MANIFEST_LEN];
            let mut pos = 0usize;
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
    // iamacoffeepot/aether#4811: the index is assigned here, at expansion time,
    // before any `#[cfg]` is evaluated — a proc macro cannot know which
    // configuration the crate is being built in. So a gated-out kind leaves a
    // hole in the suffix sequence rather than renumbering its siblings, which is
    // also the property worth having: the suffix exists only to keep these
    // statics uniquely named inside one compilation unit, nothing reads it by
    // name from outside, and a hole keeps every surviving identifier stable
    // across configurations.
    let retained_kinds: Vec<(Type, String, Vec<syn::Attribute>)> = handlers
        .iter()
        .enumerate()
        .map(|(idx, h)| (h.kind_ty.clone(), idx.to_string(), h.cfgs.clone()))
        .chain(config_kind_ty.map(|cfg| (cfg.clone(), "CONFIG".to_string(), Vec::new())))
        .collect();

    let statics = retained_kinds.iter().map(|(k, idx, cfgs)| {
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
            #(#cfgs)*
            static #schema_ident: ::aether_actor::__macro_internals::SchemaType =
                <#k as ::aether_actor::__macro_internals::Schema>::SCHEMA;
            #(#cfgs)*
            const #len_ident: usize =
                ::aether_actor::__macro_internals::canonical::canonical_len_kind(
                    <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                    &#schema_ident,
                );
            #(#cfgs)*
            const #bytes_ident: [u8; #len_ident] =
                ::aether_actor::__macro_internals::canonical::canonical_serialize_kind::<#len_ident>(
                    <#k as ::aether_actor::__macro_internals::Kind>::NAME,
                    &#schema_ident,
                );
            // Same wire shape as the Kind derive's primary emission
            // (ADR-0118 / issue 1984: the owned aether-wire encoding) so
            // retention records (when this kind lives in a dependency
            // rlib) pair cleanly with the primary records by id.
            #(#cfgs)*
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
            #(#cfgs)*
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
            #(#cfgs)*
            const #labels_len_ident: usize =
                ::aether_actor::__macro_internals::canonical::canonical_len_labels(
                    &#labels_static_ident,
                );
            #(#cfgs)*
            const #labels_bytes_ident: [u8; #labels_len_ident] =
                ::aether_actor::__macro_internals::canonical::canonical_serialize_labels::<#labels_len_ident>(
                    &#labels_static_ident,
                );
            #(#cfgs)*
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instanced_root_does_not_emit_a_lineage_anchor() {
        let self_ty = syn::parse_quote!(Probe);
        let singleton = build_actor_lineage_manifest_consts(
            &self_ty,
            &ActorOpts { cardinality: Some(ActorCardinality::Singleton), root: true, ..ActorOpts::default() },
        )
        .to_string();
        let instanced = build_actor_lineage_manifest_consts(
            &self_ty,
            &ActorOpts { cardinality: Some(ActorCardinality::Instanced), root: true, ..ActorOpts::default() },
        )
        .to_string();

        assert!(singleton.contains("write_actor_lineage_root"));
        assert!(!instanced.contains("write_actor_lineage_root"));
    }
}
