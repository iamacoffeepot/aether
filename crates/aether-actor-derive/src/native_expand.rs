use std::fs;
use std::path::Path;

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Expr, ImplItem, ItemImpl, ItemStruct, Type};

use crate::handler_parse::{
    HandlerClass, HandlerReply, HandlerVariant, NativeActorHandlerFn, NativeActorTaskHandlerFn, NativeFallbackFn,
    TaskReplyMode, attr_is_fallback, attr_is_handler, classify_handler_reply, classify_task_reply_mode,
    extract_native_actor_handler_kind, extract_task_handler_types, multi_kind_or_return_error, parse_handler_class,
    parse_handler_variant, reject_duplicate_handler_kinds, rename_lifecycle_hooks, rewrite_self_state_first_param,
    types_token_eq, validate_addressable_consts, validate_native_fallback_sig,
};
use crate::opts::{ActorCardinality, ActorOpts};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NativeEmit {
    /// Impl-hosted `#[actor] impl NativeActor for X`: the always-on addressing
    /// markers (`Addressable` / `HandlesKind` / name inventory) *plus* the
    /// runtime surface, the latter `#[cfg]`-gated (split caps gate on the
    /// runtime feature, un-split on `not(wasm)`).
    Full,
    /// Struct-hosted `#[runtime] impl NativeActor for X` (ADR-0123): only the
    /// runtime surface (`Lifecycle` / `Dispatch` / `NativeActor` / inherent
    /// handler impl), ungated — the `mod runtime;` line carries the `#[cfg]`.
    /// The addressing markers come from the struct-side `#[actor]`, so none are
    /// emitted here and the `NAMESPACE` const is consumed (dropped).
    RuntimeOnly,
}

fn reject_generic_native_lineage(generics: &syn::Generics, opts: &ActorOpts) -> syn::Result<()> {
    if generics.params.is_empty() || (!opts.root && opts.child_of.is_empty()) {
        return Ok(());
    }

    Err(syn::Error::new_spanned(
        &generics.params,
        "#[actor] `root` and `child_of(...)` require a concrete native actor identity; \
         generic native actors cannot emit monomorphic RootEntry/ChildEntry inventory facts",
    ))
}

// Emits the full `NativeActor` surface in one walk: dispatch table,
// `init` wrapper, `HandlesKind<K>` impls per handler, plus the
// dispatch ABI plumbing. Splitting into helpers would force shared
// per-handler context structs without saving readability.
#[allow(clippy::too_many_lines)]
pub fn expand_native_actor_trait(item: ItemImpl, opts: &ActorOpts, emit: NativeEmit) -> syn::Result<TokenStream2> {
    if emit == NativeEmit::Full {
        if opts.composable {
            return Err(syn::Error::new_spanned(
                &item.self_ty,
                "`composable` is available only to instanced Wasm actors; native actors must declare exact `child_of(...)` permissions",
            ));
        }
        reject_generic_native_lineage(&item.generics, opts)?;
    }

    let self_ty = &item.self_ty;
    let generics = &item.generics;
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let trait_path = item.trait_.as_ref().map(|(_, p, _)| p).expect("trait_ checked above");

    // Spike: identity/runtime split. A pre-scan for an explicit
    // `type State = …`. When present and not `Self`, the macro divides
    // its emission: the addressing markers (`Addressable` / `HandlesKind` /
    // name inventory) stay always-on against the identity `Self`, while the
    // runtime impls (`Lifecycle` / `NativeActor` / `NativeDispatch` + the
    // handler bodies) are gated behind `feature = "runtime"` and target the
    // declared state type. Absent (the shape every un-split cap keeps), the
    // macro emits `type State = Self` and the legacy `not(wasm)`-gated
    // surface unchanged.
    let declared_state_ty: Option<Type> = item.items.iter().find_map(|it| {
        if let ImplItem::Type(t) = it
            && t.ident == "State"
        {
            return Some(t.ty.clone());
        }
        None
    });
    let is_split = declared_state_ty.as_ref().is_some_and(|t| quote!(#t).to_string() != "Self");

    let mut init_method: Option<syn::ImplItemFn> = None;
    let mut config_type: Option<syn::ImplItemType> = None;
    // ADR-0156 §1/§2 (issue 3845): optional `type Params = …`. Unlike
    // `Config` (required on native), `Params` is synthesized to `()` when the
    // author omits it — the `Persist` stand-in shape — so every existing cap
    // keeps a zero-line diff. A `_params: ()` param is injected into `init`.
    let mut params_type: Option<syn::ImplItemType> = None;
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
            ImplItem::Type(it) if it.ident == "Params" => {
                params_type = Some(it);
            }
            ImplItem::Type(it) if it.ident == "State" => {
                // Pre-scanned into `declared_state_ty` above; accepted here so
                // it isn't rejected as a stray associated type.
                let _ = it;
            }
            ImplItem::Type(it) => {
                return Err(syn::Error::new_spanned(
                    it,
                    "#[actor] impl NativeActor for X accepts only `type Config = …`, \
                     `type Params = …`, and `type State = …` — other associated types \
                     aren't part of the trait",
                ));
            }
            ImplItem::Const(c) => {
                consts.push(c);
            }
            ImplItem::Fn(mut f) => {
                let handler_attr_idx = f.attrs.iter().position(attr_is_handler);
                let fallback_attr_idx = f.attrs.iter().position(attr_is_fallback);
                if handler_attr_idx.is_some() && fallback_attr_idx.is_some() {
                    return Err(syn::Error::new_spanned(&f, "method cannot be both #[handler] and #[fallback]"));
                }
                if let Some(idx) = handler_attr_idx {
                    let variant = parse_handler_variant(&f.attrs[idx])?;
                    // ADR-0112 / ADR-0134: read the reply class off the marker
                    // path. A task handler always receives the downgraded
                    // `Single` ctx, so it carries no class field.
                    let class = parse_handler_class(&f.attrs[idx], variant)?;
                    f.attrs.remove(idx);
                    match variant {
                        HandlerVariant::Mail => {
                            let (kind_ty, is_slice) = extract_native_actor_handler_kind(&f.sig, is_split)?;
                            let reply = classify_handler_reply(&f.sig.output);
                            // ADR-0134: enforce the multi-class `-> ()` return
                            // and the required `Multi<K>` ctx marker (a pointed
                            // error when absent). The native dispatch reads `K`
                            // by inference off the signature, so the extracted
                            // kind itself is not retained here.
                            multi_kind_or_return_error(class, &reply, &f.sig)?;
                            handlers.push(NativeActorHandlerFn { method: f, kind_ty, is_slice, reply, class });
                        }
                        HandlerVariant::Task => {
                            // A task handler always dispatches with the `Single`
                            // reply class — the completion reply rides `TaskDone`,
                            // not the handler class — so the `NativeActorTaskHandlerFn`
                            // carries no class field and any non-`Single` marker
                            // (e.g. `#[handler::manual(task)]`) would be silently
                            // discarded. Reject it at the boundary instead.
                            if class != HandlerClass::Single {
                                return Err(syn::Error::new_spanned(
                                    &f,
                                    "#[handler(task)] always uses the single reply class; \
                                     drop the `manual` / `multi` class marker — task replies \
                                     go through `TaskDone`, not the handler class \
                                     (ADR-0112 / ADR-0134)",
                                ));
                            }
                            let (output_ty, context_ty, is_borrow) = extract_task_handler_types(&f.sig, is_split)?;
                            let mode = classify_task_reply_mode(&f.sig, is_borrow)?;
                            task_handlers.push(NativeActorTaskHandlerFn { method: f, output_ty, context_ty, mode });
                        }
                    }
                } else if let Some(idx) = fallback_attr_idx {
                    if fallback.is_some() {
                        return Err(syn::Error::new_spanned(&f, "at most one #[fallback] method per native actor"));
                    }
                    validate_native_fallback_sig(&f.sig, is_split)?;
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

    let mut init_method = init_method.ok_or_else(|| {
        syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor requires \
             `fn init(config: Self::Config, params: Self::Params, ctx: &mut NativeInitCtx<'_>) \
             -> Result<Self, BootError>`",
        )
    })?;

    let config_type = config_type.ok_or_else(|| {
        syn::Error::new_spanned(
            self_ty,
            "#[actor] impl NativeActor requires `type Config = …` — \
             use `()` for caps without configuration",
        )
    })?;

    // ADR-0156 §2 (issue 3845): the factory becomes `init(config, params,
    // ctx)`. `Config` is required (declared above), so the user always spells
    // `config` — index 0. When the author omits `type Params`, synthesize
    // `type Params = ();` and inject a `_params: ()` at index 1, so an
    // existing cap's `fn init(config, ctx)` body still satisfies the new
    // trait shape with a zero-line author diff.
    let synthesized_params_type = if params_type.is_some() {
        None
    } else {
        let synth: syn::ImplItemType = syn::parse_quote!(
            type Params = ();
        );
        let params_param: syn::FnArg = syn::parse_quote!(_params: ());
        init_method.sig.inputs.insert(1, params_param);
        Some(synth)
    };
    let params_type_tokens = match (params_type.as_ref(), synthesized_params_type.as_ref()) {
        (Some(user), _) => quote! { #user },
        (None, Some(synth)) => quote! { #synth },
        (None, None) => unreachable!("synthesized_params_type is Some when user omitted"),
    };

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
        if let Some(earlier) =
            task_handlers[..i].iter().find(|earlier| types_token_eq(&earlier.output_ty, &later.output_ty))
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
    reject_duplicate_handler_kinds(&handlers)?;

    // `NAMESPACE` is declared on the supertrait `Addressable`, but the user
    // wrote it inside `impl NativeActor for X` for the symmetric
    // authoring shape. Route the const onto a sibling `impl Addressable for X`
    // block so satisfying the supertrait bound works without making the
    // user split the impl.
    //
    // Validate the const surface in one pass. Dispatch placement is no
    // longer authorable (issue 1187): the scheduling enum + trait const
    // were removed — every actor drains on the chassis worker pool, so a
    // leftover `SCHEDULING` const earns a pointed diagnostic. Any const
    // other than `NAMESPACE` is stray (the `Addressable` super-trait carries no
    // other authorable const) and is rejected at its own span rather than
    // silently routed onto the sibling `impl Addressable` block; and the
    // presence of `NAMESPACE` is tracked so a block that omits it fails
    // here (spanned at the type) instead of at a later "no associated const
    // NAMESPACE" error against the surfaceless `Addressable` trait.
    // NAMESPACE passes through unchanged because its RHS is a primitive that
    // doesn't require resolution. The `Addressable` body restates the const
    // (built inside the helper) so the struct-hosted path (which has only the
    // harvested expr) and this path share one emission helper.
    let namespace_expr: &Expr = validate_addressable_consts(&consts, self_ty, "NativeActor")?;

    // ADR-0122 / ADR-0123: the always-on addressing markers (`Addressable` /
    // per-kind `HandlesKind` / name inventory) are shared with the struct-hosted
    // `#[actor]` path. The impl-hosted form emits them here (`Full`); the
    // `#[runtime]` form (`RuntimeOnly`) leaves them to the struct's `#[actor]`.
    let markers = match emit {
        NativeEmit::Full => {
            // Mail handlers only — task completions get no `HandlesKind` /
            // name-inventory entry (a completion is not inbound mail).
            let handler_kinds: Vec<HandlerMarker> =
                handlers.iter().map(|h| (h.kind_ty.clone(), h.reply.manifest_kind().cloned())).collect();
            emit_native_identity_markers(
                self_ty,
                generics,
                namespace_expr,
                opts,
                &handler_kinds,
                fallback.is_some(),
                // The name-inventory entry is split-only (an un-split cap
                // registers its name through the chassis builder, not the macro).
                is_split,
            )
        }
        NativeEmit::RuntimeOnly => quote! {},
    };

    // ADR-0081 retired the chassis-pushed `ConfigureLogDrain` mail;
    // see the matching note on the `#[actor]` derive path.

    // Folded shape: the handler / fallback / wire / unwire / task bodies are
    // emitted into the identity's *inherent* impl, and `NativeActor`'s
    // associated fns forward to them via UFCS. Two rewrites prepare them:
    //
    // 1. (split only) the inherent impl sits on the identity, where the bare
    //    `Self::State` a split handler is authored with is an ambiguous
    //    associated type — substitute it with the concrete state type.
    // 2. `wire` / `unwire` collide by name with the `NativeActor` trait fns,
    //    so rename the inherent copies to `__aether_wire` / `__aether_unwire`;
    //    the trait `wire` / `unwire` forward to them. (`init` does not need a
    //    rename: it is emitted directly as the trait method.)
    if is_split && let Some(concrete) = declared_state_ty.as_ref() {
        for h in &mut handlers {
            rewrite_self_state_first_param(&mut h.method, concrete);
        }
        for t in &mut task_handlers {
            rewrite_self_state_first_param(&mut t.method, concrete);
        }
        if let Some(f) = fallback.as_mut() {
            rewrite_self_state_first_param(&mut f.method, concrete);
        }
        for m in &mut lifecycle_methods {
            rewrite_self_state_first_param(m, concrete);
        }
    }
    let (has_wire, has_unwire) = rename_lifecycle_hooks(&mut lifecycle_methods);

    // The concrete runtime state type: the declared `type State` for a split
    // cap, `Self` for an un-split one. The composed `Lifecycle<S>` /
    // `Dispatch<S>` impls carry no `NativeActor` bound, so their method
    // signatures must name this concrete type rather than `Self::State`.
    let state_ty: Type = if is_split {
        declared_state_ty.expect("is_split implies a declared `type State`")
    } else {
        syn::parse_quote!(Self)
    };

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
        // Folded shape: dispatch is a `NativeActor` associated fn over
        // `__aether_state: &mut Self::State`, and every handler is an
        // inherent item on the identity. Call uniformly through UFCS
        // `Self::on_x(state, …)` — for an un-split cap (`State = Self`) the
        // handler is a `&mut self` method and UFCS passes `state` as the
        // receiver; for a split cap it is an associated fn taking the state
        // explicitly. One call form covers both.
        let call = match (h.class, &h.reply) {
            (HandlerClass::Single, HandlerReply::Sync(_)) => quote! {
                let __aether_reply = #self_ty::#method_ident(
                    __aether_state, __aether_ctx.as_single(), __aether_decoded);
                ::aether_actor::OutboundReply::reply(__aether_ctx, &__aether_reply);
            },
            (HandlerClass::Single, HandlerReply::None | HandlerReply::Deferred(_)) => quote! {
                #self_ty::#method_ident(__aether_state, __aether_ctx.as_single(), __aether_decoded);
            },
            (HandlerClass::Manual, _) => quote! {
                #self_ty::#method_ident(__aether_state, __aether_ctx, __aether_decoded);
            },
            // ADR-0134: a multi handler is called with the `Multi<K>` view
            // (`K` inferred from its ctx signature); it emits 0..n mails and
            // returns `()`, so there is no auto-reply.
            (HandlerClass::Multi, _) => quote! {
                #self_ty::#method_ident(__aether_state, __aether_ctx.as_multi(), __aether_decoded);
            },
        };
        if h.is_slice {
            // Slice handler — payload is `count * size_of::<K>()`
            // contiguous bytes (ADR-0019 batch wire). Cast to `&[K]`
            // for the handler. Only meaningful for cast-shape kinds;
            // structured kinds have no batched wire shape.
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
            // Folded shape: UFCS `Self::method(state, …)` (see the mail-arm
            // note above) over `__aether_state: &mut Self::State`.
            let dispatch = match t.mode {
                TaskReplyMode::ByValue => quote! {
                    #self_ty::#method_ident(__aether_state, __aether_ctx.as_single(), __aether_done);
                },
                TaskReplyMode::BorrowReply => quote! {
                    let __aether_reply = #self_ty::#method_ident(
                        __aether_state, __aether_ctx.as_single(), &__aether_done);
                    __aether_done.resolve_value(__aether_ctx.as_single(), &__aether_reply);
                },
                TaskReplyMode::BorrowNoReply => quote! {
                    #self_ty::#method_ident(__aether_state, __aether_ctx.as_single(), &__aether_done);
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
    let task_handler_methods: Vec<&syn::ImplItemFn> = task_handlers.iter().map(|t| &t.method).collect();
    let fallback_method = fallback.as_ref().map(|f| &f.method);
    let helper_methods = helpers.iter();

    // Issue 576: catch-all caps override `__aether_dispatch_fallback`
    // (the default-method on `NativeDispatch` returns `false`). The
    // strict-receiver path keeps the default. Catch-all caps also
    // emit an empty `__aether_dispatch_envelope` since there are no
    // typed handlers — the trampoline routes straight to the fallback
    // override on every envelope.
    // Folded shape: a `#[fallback]` overrides `NativeActor::dispatch_fallback`
    // (the default returns `false`). Forwarded through UFCS over the state,
    // mirroring the typed-handler arms; the `#[fallback]` keeps its
    // `NativeCtx<'_>` (= Single) signature, so downgrade.
    let fallback_dispatch_override = fallback.as_ref().map(|f| {
        let method_ident = &f.method.sig.ident;
        quote! {
            fn dispatch_fallback(
                __aether_state: &mut #state_ty,
                __aether_ctx: &mut ::aether_substrate::NativeCtx<'_, ::aether_actor::Manual>,
                __aether_env: &::aether_substrate::actor::native::envelope::Envelope,
            ) -> bool {
                #self_ty::#method_ident(__aether_state, __aether_ctx.as_single(), __aether_env);
                true
            }
        }
    });

    // iamacoffeepot/aether#1037: override `NativeActor::capabilities`
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
        fn capabilities() -> ::aether_substrate::actor::native::ComponentCapabilities {
            ::aether_substrate::actor::native::ComponentCapabilities {
                handlers: ::std::vec![#(#capability_handler_entries),*],
                fallback: #capability_fallback,
                doc: ::core::option::Option::None,
                // ADR-0090 (issue 1257): native chassis caps don't carry
                // a describe-surfaced boot-config kind.
                config: ::core::option::Option::None,
                // ADR-0163 §3: assets ride wasm custom sections, so a
                // native cap always has an empty catalog.
                assets: ::std::vec![],
            }
        }
    };

    // Issue 552 stage 4: NativeActor + NativeDispatch + the inherent
    // handler-method impl all reach for `::aether_substrate::*` paths
    // and native-only types in their bodies. They're emitted under
    // `#[cfg(not(target_family = "wasm"))]` so a cap crate
    // can compile for `wasm32-unknown-unknown` without the substrate
    // dep — wasm consumers see only the always-on Addressable +
    // HandlesKind markers, which is enough for typed
    // `ctx.actor::<R>().send(...)` against cap markers.
    //
    // Gate is `target_arch` not `feature = "runtime"` because
    // NativeActor/NativeDispatch are wasm-incompatible by definition;
    // there's no realistic case where a host build wants to skip
    // them. Pinning the cfg in the macro means consumer crates never
    // have to define matching feature flags.
    // Spike: identity/runtime split keying.
    //
    // - Legacy (un-split): the identity IS its runtime, so the runtime impls
    //   target `#self_ty` and gate on `not(wasm)` exactly as before. The
    //   `NativeActor` impl pins `type State = Self`.
    // - Split (`type State = …`): the runtime impls (`Lifecycle` /
    //   `NativeDispatch`) target the declared state type and gate behind
    //   `feature = "runtime"` (the hardcoded convention for now), so a
    //   transport-only build never names the state type or pulls
    //   `aether_substrate`. The handler inherent impl stays on the identity
    //   (its assoc fns take `state: &mut Self::State`). The `NativeActor`
    //   impl pins `type State` to the declared type and bridges identity →
    //   runtime. The addressing markers above (`Addressable` /
    //   `HandlesKind`) are always-on regardless.
    // iamacoffeepot/aether#2330: a split cap gates its runtime impls behind the
    // generic `runtime` feature by default, or a cap-specific feature when
    // `#[actor(runtime_feature = "name")]` overrides it (the media caps whose
    // native half lives behind `render-runtime` / `audio-runtime` / … name it so
    // a plain-`runtime` build never tries to compile their substrate-typed
    // impls without the heavy dep).
    let runtime_gate = match emit {
        // ADR-0123: `#[runtime]` emits the runtime surface ungated — the
        // `#[cfg]` rides the author-written `mod runtime;` line, so the impls
        // already only exist in a build where the runtime module is present.
        NativeEmit::RuntimeOnly => quote! {},
        NativeEmit::Full if is_split => {
            if let Some(feat) = opts.runtime_feature.as_deref() {
                quote! { #[cfg(feature = #feat)] }
            } else {
                quote! { #[cfg(feature = "runtime")] }
            }
        }
        NativeEmit::Full => quote! { #[cfg(not(target_family = "wasm"))] },
    };

    // Composed shape: `Lifecycle<S>::wire` / `unwire` forward to the inherent
    // `__aether_{wire,unwire}` copies (renamed above to dodge the trait-name
    // collision) by UFCS — passing the state as the receiver for an un-split
    // `&mut self` hook. Emitted only when the user provided the hook; the
    // trait's default no-op stands otherwise.
    let wire_forward = if has_wire {
        quote! {
            fn wire(
                __aether_state: &mut #state_ty,
                __aether_ctx: &mut ::aether_substrate::NativeCtx<'_>,
            ) {
                #self_ty::__aether_wire(__aether_state, __aether_ctx);
            }
        }
    } else {
        quote! {}
    };
    let unwire_forward = if has_unwire {
        quote! {
            fn unwire(
                __aether_state: &mut #state_ty,
                __aether_ctx: &mut ::aether_substrate::NativeCtx<'_>,
            ) {
                #self_ty::__aether_unwire(__aether_state, __aether_ctx);
            }
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        // Always-on addressing markers (`Full` only): the identity carries
        // `Addressable` (`NAMESPACE` / `Resolver`), the per-handler
        // `HandlesKind<K>` impls, and (split caps) the name-inventory entry —
        // none of which name the runtime state or pull `aether_substrate`. On
        // the `RuntimeOnly` (`#[runtime]`) path this is empty: the struct-side
        // `#[actor]` emits the markers.
        #markers

        // Composed shape: the addressing identity carries the two native
        // behaviour traits parameterised by the runtime state, plus the
        // `NativeActor` composition that pins `State` (plain data). No
        // behaviour trait is implemented on the state itself.

        // Boot lifecycle over the state — the shared `aether_actor::Lifecycle<S>`
        // (iamacoffeepot/aether#2311), with the per-target ctx GATs pinned to
        // the concrete native ctx types so an `init`/`wire` body keeps its
        // concrete ctx.
        #runtime_gate
        impl #impl_generics ::aether_actor::Lifecycle<#state_ty> for #self_ty #where_clause {
            #config_type
            #params_type_tokens
            type InitError = ::aether_substrate::BootError;
            type InitCtx<'__a> = ::aether_substrate::NativeInitCtx<'__a>;
            type Ctx<'__a> = ::aether_substrate::NativeCtx<'__a>;
            #init_method
            #wire_forward
            #unwire_forward
        }

        // Per-kind dispatch over the state.
        #runtime_gate
        impl #impl_generics ::aether_substrate::actor::native::Dispatch<#state_ty>
            for #self_ty #where_clause
        {
            // ADR-0112: the dispatch seam carries the most-permissive
            // `Manual` ctx; the arms downgrade per handler class.
            fn dispatch(
                __aether_state: &mut #state_ty,
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

        // The composition: identity → runtime state.
        #runtime_gate
        impl #impl_generics #trait_path for #self_ty #where_clause {
            type State = #state_ty;
        }

        // The handler / fallback / wire / unwire / helper bodies as inherent
        // items on the identity. The trait fns above reach them by UFCS:
        // `Self::on_x(state, …)` — for an un-split cap (`State = Self`) `state`
        // lands as the `&mut self` receiver; for a split cap the items are
        // associated fns taking the state explicitly.
        #runtime_gate
        impl #impl_generics #self_ty #where_clause {
            #(#handler_methods)*
            #(#task_handler_methods)*
            #fallback_method
            #(#lifecycle_methods)*
            #(#helper_methods)*
        }
    })
}

/// One mail handler's marker payload: its inbound kind type plus the manifest
/// reply kind read off the handler's return type (`None` for `-> ()`).
type HandlerMarker = (Type, Option<Type>);

/// Emit ADR-0166 placement marker impls and their native link-time inventory
/// records. Generic identities with lineage declarations are rejected before
/// this helper so every emitted marker has a corresponding inventory fact.
fn emit_native_lineage_markers(self_ty: &Type, generics: &syn::Generics, opts: &ActorOpts) -> TokenStream2 {
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let root_impl = opts.root.then(|| {
        quote! {
            impl #impl_generics ::aether_actor::Root for #self_ty #where_clause {}
        }
    });
    let child_impls = opts.child_of.iter().map(|parent| {
        quote! {
            impl #impl_generics ::aether_actor::ChildOf<#parent>
                for #self_ty #where_clause {}
        }
    });
    let root_entry = opts.root.then(|| {
        quote! {
            #[cfg(not(target_family = "wasm"))]
            ::aether_data::name_inventory::inventory::submit! {
                ::aether_data::name_inventory::RootEntry {
                    actor: ::aether_data::ActorId::singleton(
                        <#self_ty as ::aether_actor::Addressable>::NAMESPACE,
                    ),
                    namespace: <#self_ty as ::aether_actor::Addressable>::NAMESPACE,
                }
            }
        }
    });
    let child_entries = opts.child_of.iter().map(|parent| {
        quote! {
            #[cfg(not(target_family = "wasm"))]
            ::aether_data::name_inventory::inventory::submit! {
                ::aether_data::name_inventory::ChildEntry {
                    parent: ::aether_data::ActorId::singleton(
                        <#parent as ::aether_actor::Addressable>::NAMESPACE,
                    ),
                    child: ::aether_data::ActorId::singleton(
                        <#self_ty as ::aether_actor::Addressable>::NAMESPACE,
                    ),
                    parent_namespace: <#parent as ::aether_actor::Addressable>::NAMESPACE,
                    child_namespace: <#self_ty as ::aether_actor::Addressable>::NAMESPACE,
                }
            }
        }
    });
    let inventory = quote! {
        #root_entry
        #(#child_entries)*
    };

    quote! {
        #root_impl
        #(#child_impls)*
        #inventory
    }
}

/// Emit the always-on native addressing markers shared by the impl-hosted
/// `#[actor]` path (ADR-0122) and the struct-hosted one (ADR-0123): the
/// `Addressable` impl (`NAMESPACE` + the cardinality resolver), one
/// `HandlesKind<K>` per mail handler (a single blanket impl for a
/// fallback-only catch-all cap), the cardinality-keyed name-inventory entry
/// (`emit_name_entry`), and the per-handler `HandlerEntry` inventory
/// submissions. None of these name the runtime state or pull `aether_substrate`,
/// so they compile in a transport-only / wasm build where the runtime module is
/// stripped. `handler_kinds` carries each mail handler's `(kind, reply)`; task
/// completions are not inbound mail and carry no marker.
fn emit_native_identity_markers(
    self_ty: &Type,
    generics: &syn::Generics,
    namespace_expr: &Expr,
    opts: &ActorOpts,
    handler_kinds: &[HandlerMarker],
    has_fallback: bool,
    emit_name_entry: bool,
) -> TokenStream2 {
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();

    // ADR-0119: cardinality picks the resolver — `One` (default / `singleton`)
    // or `Many` (`instanced`). `Singleton` / `Instanced` derive from it.
    let resolver_ty = if matches!(opts.cardinality, Some(ActorCardinality::Instanced)) {
        quote! { ::aether_actor::Many }
    } else {
        quote! { ::aether_actor::One }
    };
    // The `NAMESPACE` const restated in the `Addressable` body — built here so
    // both call sites pass just the expr.
    let actor_impl = quote! {
        impl #impl_generics ::aether_actor::Addressable for #self_ty #where_clause {
            const NAMESPACE: &'static str = #namespace_expr;
            type Resolver = #resolver_ty;
        }
    };
    let lineage_markers = emit_native_lineage_markers(self_ty, generics, opts);

    // Issue 576 + issue 603: a fallback-only (true catch-all) cap emits a single
    // blanket `impl<K: Kind> HandlesKind<K>` so any typed
    // `ctx.actor::<X>().send(&payload)` compiles for every K. Strict / hybrid
    // caps keep per-handler impls — only declared kinds compile via typed sends.
    let handles_kind_impls: Vec<TokenStream2> = if has_fallback && handler_kinds.is_empty() {
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
            .map(|(kind_ty, _)| {
                quote! {
                    impl #impl_generics ::aether_actor::HandlesKind<#kind_ty>
                        for #self_ty #where_clause {}
                }
            })
            .collect()
    };

    // A split cap carries its own always-on name-inventory submission, keyed by
    // cardinality off the `NAMESPACE` expr and gated `not(wasm)` so it rides the
    // transport build but never the wasm header build.
    let name_entry = if !emit_name_entry {
        quote! {}
    } else if matches!(opts.cardinality, Some(ActorCardinality::Instanced)) {
        quote! {
            #[cfg(not(target_family = "wasm"))]
            ::aether_data::name_inventory::inventory::submit! {
                ::aether_data::name_inventory::TemplateEntry {
                    domain: ::aether_data::MAILBOX_DOMAIN,
                    prefix: #namespace_expr,
                    template: ":{subname}",
                    param: ::aether_data::name_inventory::ParamKind::Dynamic,
                }
            }
        }
    } else {
        quote! {
            #[cfg(not(target_family = "wasm"))]
            ::aether_data::name_inventory::inventory::submit! {
                ::aether_data::name_inventory::NameEntry {
                    domain: ::aether_data::MAILBOX_DOMAIN,
                    name: #namespace_expr,
                }
            }
        }
    };

    // ADR-0109 §5: the native analogue of the wasm `aether.kinds.inputs` custom
    // section — one link-time `HandlerEntry` per mail handler (owning
    // `NAMESPACE`, input kind id + name, reply kind id off the return type),
    // gated `not(wasm32)`. Skipped for a generic native actor (none exist):
    // `<Self as Addressable>::NAMESPACE` wouldn't const-resolve in the
    // non-generic inventory static.
    let handler_inventory = if generics.params.is_empty() {
        let submissions = handler_kinds.iter().map(|(kind_ty, reply)| {
            let reply_expr = if let Some(reply_ty) = reply {
                quote! { ::core::option::Option::Some(<#reply_ty as ::aether_data::Kind>::ID) }
            } else {
                quote! { ::core::option::Option::None }
            };
            quote! {
                #[cfg(not(target_family = "wasm"))]
                ::aether_data::name_inventory::inventory::submit! {
                    ::aether_data::name_inventory::HandlerEntry {
                        namespace: <#self_ty as ::aether_actor::Addressable>::NAMESPACE,
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
    quote! {
        #actor_impl
        #lineage_markers
        #(#handles_kind_impls)*
        #name_entry
        #handler_inventory
    }
}

/// ADR-0123 struct-hosted `#[actor]`: `#[actor(<cardinality>[, <module>])]` on
/// the capability *struct*. It reads the runtime module off disk — a module
/// path resolved relative to the invoking file, default the sibling `runtime`,
/// nested for a headless companion (`runtime::headless`) — lifts the native
/// cap's identity out of the
/// `#[handler]`-bearing `impl NativeActor` there, and emits the always-on
/// addressing markers against the struct — passing the struct itself through
/// unchanged. The behaviour + state stay in the runtime module (under
/// `#[runtime]`); none of the emitted markers name the runtime state or pull
/// `aether_substrate`, so the identity survives a `--no-default-features` build
/// where `mod runtime` is `#[cfg]`-stripped.
pub fn expand_struct_hosted_actor(item: &ItemStruct, opts: &ActorOpts) -> syn::Result<TokenStream2> {
    if opts.composable {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "`composable` is available only to instanced Wasm actors; native actors must declare exact `child_of(...)` permissions",
        ));
    }
    reject_generic_native_lineage(&item.generics, opts)?;

    let ident = &item.ident;
    let (_impl_generics, ty_generics, _where_clause) = item.generics.split_for_impl();
    let self_ty: Type = syn::parse_quote!(#ident #ty_generics);

    // The runtime module path segments (default `runtime`) plus a span in the
    // invoking file to resolve them from. A defaulted module has no ident of
    // its own, so borrow the struct ident's span — it lives in the same file.
    let (module_segments, module_span) = match &opts.runtime_module {
        Some(path) => (
            path.segments.iter().map(|seg| seg.ident.to_string()).collect::<Vec<_>>(),
            path.segments.last().expect("syn::Path has at least one segment").ident.span(),
        ),
        None => (vec!["runtime".to_string()], ident.span()),
    };

    let (namespace_expr, handler_kinds, has_fallback, runtime_path) =
        harvest_runtime_identity(&module_segments, module_span)?;

    let markers = emit_native_identity_markers(
        &self_ty,
        &item.generics,
        &namespace_expr,
        opts,
        &handler_kinds,
        has_fallback,
        // The struct-hosted form is always a split identity, so it always
        // carries its own name-inventory entry.
        true,
    );

    // ADR-0123 gap 3: a compile-time dependency edge on the runtime file so a
    // transport-only (runtime-off) build — where `mod runtime` is cfg-stripped
    // and so is not itself a compilation input — re-runs this harvest when the
    // runtime module changes on disk. `include_bytes!` is the stable substitute
    // for the unstable `proc_macro::tracked_path` API; the absolute path is
    // emitted ungated so it rides the transport build where the staleness lives
    // (in a runtime-on build the module is already a fingerprint input, so the
    // edge is redundant but harmless).
    let runtime_path_lit = syn::LitStr::new(&runtime_path, proc_macro2::Span::call_site());

    Ok(quote! {
        #item
        #markers
        const _: &[u8] = include_bytes!(#runtime_path_lit);
    })
}

/// Read the runtime module file off disk and lift the native cap's
/// identity out of its `#[handler]`-bearing `impl NativeActor` block: the
/// `NAMESPACE` const expression, each *mail* handler's `(kind, reply)`, and
/// whether a `#[fallback]` is present. The read is cfg-blind — `syn` does not
/// evaluate `cfg`, so the identity is harvested even when `mod runtime` is
/// stripped from the build. `module_segments` is the `::`-split module path
/// resolved relative to the invoking file — `["runtime"]` reads the sibling
/// `runtime.rs` / `runtime/mod.rs`, `["runtime", "headless"]` reads
/// `runtime/headless.rs` / `runtime/headless/mod.rs`. `module_span` both
/// resolves the on-disk path (`Span::local_file`) and anchors every diagnostic
/// back at the `#[actor]` invocation rather than into the parsed (span-less)
/// runtime file.
fn harvest_runtime_identity(
    module_segments: &[String],
    module_span: proc_macro2::Span,
) -> syn::Result<(Expr, Vec<HandlerMarker>, bool, String)> {
    // `Span::local_file()` (stable since 1.88) → the on-disk path of the file
    // holding the `#[actor]` invocation. It is `None` only under path remapping
    // (`--remap-path-prefix`), where the runtime file can't be located — a hard
    // error, per ADR-0123's recorded fallback.
    let Some(decl_path) = module_span.unwrap().local_file() else {
        return Err(syn::Error::new(
            module_span,
            "#[actor]: Span::local_file() returned None — the source path is unavailable \
             (path remapping?), so the runtime module file can't be located. The struct-hosted \
             identity form needs an un-remapped build (ADR-0123).",
        ));
    };
    let dir = decl_path.parent().unwrap_or_else(|| Path::new("."));
    let module_name = module_segments.join("::");
    let (leaf, parents) = module_segments.split_last().expect("runtime module path has at least one segment");
    let base = parents.iter().fold(dir.to_path_buf(), |acc, seg| acc.join(seg));
    let flat = base.join(format!("{leaf}.rs"));
    let nested = base.join(leaf).join("mod.rs");
    let target = if flat.exists() {
        flat
    } else {
        nested
    };

    let src = fs::read_to_string(&target).map_err(|e| {
        syn::Error::new(
            module_span,
            format!(
                "#[actor]: cannot read runtime module `{module_name}` (expected \
                 `{}.rs` or `{}/mod.rs` relative to this file): {e}",
                module_segments.join("/"),
                module_segments.join("/"),
            ),
        )
    })?;
    let parsed = syn::parse_file(&src).map_err(|e| {
        syn::Error::new(module_span, format!("#[actor]: parse error in runtime module `{module_name}`: {e}"))
    })?;

    // ADR-0123 gap 3: the absolute path of the file just read, for the
    // `include_bytes!` rebuild edge the caller emits. Canonicalize (the read
    // above proved the file exists) so the emitted literal resolves independent
    // of `include_bytes!`'s span-relative base; fall back to `target` as-is on a
    // canonicalize failure.
    let runtime_path = fs::canonicalize(&target).unwrap_or(target).to_string_lossy().into_owned();

    // ADR-0123 gap 1: select the runtime impl by trait, not by handler-presence
    // alone — mirror the dispatch-side last-segment match (`impl NativeActor for
    // …`) so any import style resolves. `syn` does not evaluate `cfg`, so two
    // cfg-gated `impl NativeActor` blocks in one file are indistinguishable to
    // this parse; collect every qualifying impl and refuse rather than silently
    // pick the first.
    let mut qualifying: Vec<(Expr, Vec<HandlerMarker>, bool)> = Vec::new();
    for syn_item in &parsed.items {
        let syn::Item::Impl(imp) = syn_item else {
            continue;
        };
        let is_native_actor = imp
            .trait_
            .as_ref()
            .and_then(|(_, path, _)| path.segments.last())
            .is_some_and(|seg| seg.ident == "NativeActor");
        if !is_native_actor {
            continue;
        }
        if let Some(identity) = harvest_native_actor_impl(imp, &module_name, module_span)? {
            qualifying.push(identity);
        }
    }

    // Exactly one qualifying impl is harvested; zero and two-or-more are the
    // gap-1 diagnostics (nothing to lift / cfg-blind ambiguity).
    let mut qualifying = qualifying.into_iter();
    match (qualifying.next(), qualifying.next()) {
        (None, _) => Err(syn::Error::new(
            module_span,
            format!(
                "#[actor]: no `#[handler]`-bearing impl found in runtime module `{module_name}` — \
                 the struct-hosted form expects a sibling `#[runtime] impl NativeActor` with at \
                 least one `#[handler]` (or a `#[fallback]`)"
            ),
        )),
        (Some((namespace_expr, handler_kinds, has_fallback)), None) => {
            Ok((namespace_expr, handler_kinds, has_fallback, runtime_path))
        }
        (Some(_), Some(_)) => Err(syn::Error::new(
            module_span,
            format!(
                "#[actor]: more than one `#[handler]`-bearing `impl NativeActor` in runtime \
                 module `{module_name}` — the cfg-blind harvest can't choose between cfg-gated \
                 runtime impls, so it refuses rather than silently taking the first"
            ),
        )),
    }
}

/// Scan a single `impl NativeActor` block for the cap identity: the per-mail
/// handler kinds (`(kind, reply)`, task completions skipped), whether a
/// `#[fallback]` is present, and the `const NAMESPACE` expression. `Ok(None)`
/// means the impl hosts neither a `#[handler]` nor a `#[fallback]`, so it lifts
/// no identity; `Err` means it is handler-bearing but omits `const NAMESPACE`
/// (or a handler signature is malformed). `module_span` anchors diagnostics back
/// at the `#[actor]` invocation.
fn harvest_native_actor_impl(
    imp: &ItemImpl,
    module_name: &str,
    module_span: proc_macro2::Span,
) -> syn::Result<Option<(Expr, Vec<HandlerMarker>, bool)>> {
    let remap = |e: syn::Error| {
        syn::Error::new(module_span, format!("#[actor]: harvesting runtime module `{module_name}`: {e}"))
    };

    let mut handler_kinds: Vec<(Type, Option<Type>)> = Vec::new();
    let mut has_fallback = false;
    let mut saw_handler = false;
    for impl_item in &imp.items {
        let ImplItem::Fn(f) = impl_item else {
            continue;
        };
        if f.attrs.iter().any(attr_is_fallback) {
            has_fallback = true;
            continue;
        }
        let Some(handler_attr) = f.attrs.iter().find(|a| attr_is_handler(a)) else {
            continue;
        };
        saw_handler = true;
        // Task completions get no `HandlesKind` / inventory marker.
        if matches!(parse_handler_variant(handler_attr).map_err(remap)?, HandlerVariant::Task) {
            continue;
        }
        let (kind_ty, _is_slice) = extract_native_actor_handler_kind(&f.sig, true).map_err(remap)?;
        let reply = classify_handler_reply(&f.sig.output).manifest_kind().cloned();
        handler_kinds.push((kind_ty, reply));
    }

    // A `NativeActor` impl with neither a `#[handler]` nor a `#[fallback]`
    // carries no identity to lift.
    if !saw_handler && !has_fallback {
        return Ok(None);
    }
    let namespace_expr = imp.items.iter().find_map(|impl_item| match impl_item {
        ImplItem::Const(c) if c.ident == "NAMESPACE" => Some(c.expr.clone()),
        _ => None,
    });
    let Some(namespace_expr) = namespace_expr else {
        return Err(syn::Error::new(
            module_span,
            format!(
                "#[actor]: the runtime impl in module `{module_name}` has #[handler]s but \
                 no `const NAMESPACE` to lift into Addressable"
            ),
        ));
    };
    Ok(Some((namespace_expr, handler_kinds, has_fallback)))
}
