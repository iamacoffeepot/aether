use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{FnArg, ImplItem, ItemImpl, Type};

use crate::diagnostics::{doc_attrs, extract_agent_doc};
use crate::export_desc::emit_actor_export_desc;
use crate::handler_parse::{
    FallbackFn, HandlerClass, HandlerFn, HandlerReply, HandlerVariant, IntentParameters, SenderArm, WatchHandlerFn,
    allow_abi_receiver, allow_context_by_value, attr_is_fallback, attr_is_handler, check_intent_signature,
    check_watch_signature, classify_handler_reply, ctx_names_actor, departed_watched_type, extract_handler_kind_type,
    fill_ctx_actor, handler_cfgs, parse_handler_args, parse_handler_class, reject_ctx_sender,
    reject_duplicate_handler_kinds, reject_duplicate_watched_types, rename_lifecycle_hooks, require_wire_result,
    sender_arm, silent_call, validate_addressable_consts, validate_fallback_sig,
};
use crate::manifest::{
    build_actor_lineage_manifest_consts, build_inputs_manifest_consts, build_kinds_section_retention_statics,
};
use crate::opts::{ActorCardinality, ActorOpts};
use crate::reply_markers::{
    ContractRow, DeclaredLists, ReplyMarkerSite, RowSpec, conjoined_cfg_predicate, contract_element, contract_row_impl,
    contract_rows_expr, contracts_impl, declared_impl, handles_kind_impl, position, refusal_answer, reply_marker_impl,
    rows_list, watchable_actor_impl,
};

/// Wasm-actor expansion — `#[actor] impl WasmActor for X` (or
/// the back-compat `impl Component for X`). Emits the full wasm
/// surface: dispatch table referencing `aether_actor::WasmCtx<'_>`,
/// init wrapper, `aether.kinds.inputs` manifest consts, kind retention
/// statics, plus the `HandlesKind<K>` and `Addressable` impls common to both
/// shapes.
#[allow(clippy::too_many_lines)] // emits the full wasm-actor surface in one go
pub fn expand_wasm_actor(item: ItemImpl, opts: &ActorOpts) -> syn::Result<TokenStream2> {
    let self_ty = &item.self_ty;
    if !opts.child_of.is_empty() && !matches!(opts.cardinality, Some(ActorCardinality::Instanced)) {
        return Err(syn::Error::new_spanned(
            self_ty,
            "`child_of(...)` requires explicit instanced Wasm cardinality; \
             use `#[actor(instanced, child_of(Parent))]`",
        ));
    }

    let generics = &item.generics;
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let trait_path = item.trait_.as_ref().map(|(_, p, _)| p).expect("trait_ checked above");

    let component_doc = extract_agent_doc(&item.attrs);
    let impl_docs = doc_attrs(&item.attrs);

    let mut init_method: Option<syn::ImplItemFn> = None;
    let mut lifecycle_methods: Vec<syn::ImplItemFn> = Vec::new();
    let mut handlers: Vec<HandlerFn> = Vec::new();
    // ADR-0079 §8: the departure handlers, which share one row, and the slot
    // in `handlers` that row takes: where the first of them was declared.
    let mut watch_handlers: Vec<WatchHandlerFn> = Vec::new();
    let mut watch_slot: Option<usize> = None;
    let mut fallback: Option<FallbackFn> = None;
    let mut helpers: Vec<syn::ImplItemFn> = Vec::new();
    // Issue 525 Phase 1B: pass-through trait consts (today just
    // NAMESPACE) so each component declares them inside its
    // `#[actor] impl WasmActor for C` block alongside `init` /
    // `#[handler]` methods.
    let mut consts: Vec<syn::ImplItemConst> = Vec::new();
    // ADR-0090 (issue 1256): optional `type Config = …` declaration.
    // When omitted, the macro synthesizes `type Config = ();` so the
    // emitted `export!` shim can decode 0 config bytes via
    // `impl Kind for ()` and the user's `init` body stays 1-param.
    let mut config_type: Option<syn::ImplItemType> = None;
    // ADR-0156 §1/§2 (issue 3845): optional `type Params = …` declaration.
    // When omitted, the macro synthesizes `type Params = ();` and injects a
    // `_params: ()` leading param into `init` — the same stand-in the `Config`
    // slot uses — so the author's `init` body stays unchanged and the emitted
    // shim resolves 0 params bytes via `impl Kind for ()`.
    let mut params_type: Option<syn::ImplItemType> = None;
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
            ImplItem::Type(it) if it.ident == "Params" => {
                params_type = Some(it);
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
                    return Err(syn::Error::new_spanned(&f, "method cannot be both #[handler] and #[fallback]"));
                }

                if let Some(idx) = handler_attr_idx {
                    // ADR-0093 §7: dispatch completions are native-only.
                    // `try_take_task_done` lives on `NativeCtx`; the
                    // wasm path has no umbrella-aware blocking
                    // dispatch yet. Reject `#[handler(task)]` here with a
                    // clear diagnostic rather than letting it expand into
                    // a guest dispatch table that can't satisfy it.
                    //
                    // ADR-0112 / ADR-0134 / #7201: the reply class and the intent
                    // are read off the marker path first, so an intent word
                    // refuses a `task` argument in its own words. An intent word
                    // may carry a fourth parameter, which
                    // `check_intent_signature` judges.
                    let args = parse_handler_args(&f.attrs[idx])?;
                    let (class, intent) = parse_handler_class(&f.attrs[idx], &args)?;
                    if args.variant == HandlerVariant::Task {
                        return Err(syn::Error::new_spanned(
                            &f,
                            "dispatch completions are native-only (ADR-0093 §7); \
                             `#[handler(task)]` is not supported in wasm components",
                        ));
                    }
                    let kind_ty = extract_handler_kind_type(&f.sig, intent.is_some())?;
                    let agent_doc = extract_agent_doc(&f.attrs);
                    let reply = classify_handler_reply(&f.sig.output);
                    // ADR-0079 §8: a handler over `Departed<W>` is a
                    // departure handler, judged by its own signature rules
                    // and kept apart from the mail handlers.
                    if let Some(watched_ty) = departed_watched_type(&kind_ty).cloned() {
                        let context_ty = check_watch_signature(&f.attrs[idx], intent, &reply, &f.sig)?;
                        let cfgs = handler_cfgs(&f.attrs);
                        f.attrs.remove(idx);
                        // The arm hands the taken context over by value, as a
                        // response arm does.
                        if context_ty.is_some() {
                            f.attrs.push(syn::parse_quote!(#[allow(clippy::needless_pass_by_value)]));
                        }
                        fill_ctx_actor(&mut f.sig);
                        allow_abi_receiver(&mut f);
                        watch_slot.get_or_insert(handlers.len());
                        watch_handlers.push(WatchHandlerFn { method: f, watched_ty, context_ty, cfgs });
                        continue;
                    }
                    let IntentParameters { response_context, sender } = intent
                        .map(|i| check_intent_signature(i, &reply, &f.sig, false))
                        .transpose()?
                        .unwrap_or_default();
                    // iamacoffeepot/aether#4811: the method keeps its own `#[cfg]`s
                    // (only the marker attribute is removed), so clone them for
                    // the artifacts derived from it.
                    let cfgs = handler_cfgs(&f.attrs);
                    f.attrs.remove(idx);
                    allow_context_by_value(&mut f.attrs, response_context.as_ref());
                    fill_ctx_actor(&mut f.sig);
                    allow_abi_receiver(&mut f);
                    handlers.push(HandlerFn {
                        method: f,
                        kind_ty,
                        agent_doc,
                        reply,
                        class,
                        unchecked_reason: args.reason,
                        cfgs,
                        response_context,
                        sender,
                    });
                } else if let Some(idx) = fallback_attr_idx {
                    if fallback.is_some() {
                        return Err(syn::Error::new_spanned(&f, "at most one #[fallback] method per component"));
                    }
                    validate_fallback_sig(&f.sig)?;
                    reject_ctx_sender(
                        &f.sig,
                        "`#[fallback]`",
                        "it catches mail no handler names, so no kind's send could carry the requirement",
                    )?;
                    let agent_doc = extract_agent_doc(&f.attrs);
                    f.attrs.remove(idx);
                    fill_ctx_actor(&mut f.sig);
                    allow_abi_receiver(&mut f);
                    fallback = Some(FallbackFn { method: f, agent_doc });
                } else if name == "init" {
                    init_method = Some(f);
                } else if matches!(name.as_str(), "wire" | "unwire" | "on_dehydrate" | "on_rehydrate") {
                    // `on_dehydrate` takes a `WasmDropCtx`, which the fill leaves
                    // alone.
                    reject_ctx_sender(&f.sig, "a lifecycle hook", "it dispatches no mail, so there is no sender")?;
                    fill_ctx_actor(&mut f.sig);
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
            "#[actor] requires `fn init(ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError>` \
             (or, with `type Config = T`, `fn init(config: T, ctx: &mut WasmInitCtx<'_>) -> …`)",
        )
    })?;

    // ADR-0079 §8: the departure handlers become one mail handler for the
    // engine's notice, at the first one's slot, so every artifact a handler
    // gets (its `HandlesKind`, contract row, manifest record, retention, and
    // dispatch arm) is emitted once for the group by the code below.
    reject_duplicate_watched_types(&watch_handlers)?;
    if let Some(slot) = watch_slot {
        if let Some(direct) = handlers.iter().find(|h| type_names_monitor_notice(&h.kind_ty)) {
            return Err(syn::Error::new_spanned(
                &direct.kind_ty,
                "this component takes its departures as `Departed<W>`, so it cannot also take `MonitorNotice`: the \
                 notice is the mail its `Departed<W>` handlers share one row for (ADR-0079 §8)",
            ));
        }
        handlers.insert(slot, departure_handler(&watch_handlers)?);
    }

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
    reject_duplicate_handler_kinds(&handlers)?;

    // ADR-0113 (issue 1855): declarative persistence. `type State` plus
    // the `dehydrate` / `rehydrate` accessor pair generate the
    // `on_dehydrate` / `on_rehydrate` hooks, so they are mutually
    // exclusive with hand-written hooks and require each other. Validate
    // the XOR at the offending span before synthesizing / generating.
    let manual_state_hook =
        lifecycle_methods.iter().find(|m| matches!(m.sig.ident.to_string().as_str(), "on_dehydrate" | "on_rehydrate"));
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

    // iamacoffeepot/aether#2311: the ADR-0113 persistence assoc type renamed
    // `State` → `Persist` (the runtime `type State` took its name). The
    // authoring keyword stays `type State = …`; route it to the `Persist`
    // slot. Mirror `synthesized_config_type`: synthesize `type Persist = ();`
    // when the author omitted it (gated on `state_type.is_some()` at macro
    // time), so a no-persistence actor keeps the default no-op hooks and pays
    // nothing.
    let persist_type_tokens: TokenStream2 = if let Some(user) = state_type.as_ref() {
        let ty = &user.ty;
        quote! { type Persist = #ty; }
    } else {
        quote! { type Persist = (); }
    };

    // ADR-0090 (issue 1256): the trait now takes `init(config: Self::Config,
    // ctx: &mut C)`. If the user declared `type Config = …`, leave their
    // init alone — they're expected to spell out the `config` param. If
    // they omitted it, the macro synthesizes `type Config = ();` AND
    // injects a `_config: ()` leading param so the user's pre-#1256 body
    // (`fn init(ctx: &mut WasmInitCtx<'_>) -> …`) keeps compiling. The emitted shim
    // always decodes `<Self as WasmActor>::Config` from bytes, so the
    // synthesized `_config: ()` path round-trips uniformly via
    // `impl Kind for ()`.
    let (synthesized_config_type, mut init_method_emitted) = if config_type.is_some() {
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
        // has no `self` receiver (WasmActor::init is associated, not a
        // method), so index 0 is the right slot.
        init_method.sig.inputs.insert(0, config_param);
        (Some(synth), init_method)
    };

    // ADR-0156 §2 (issue 3845): the trait factory is now
    // `init(config, params, ctx)`. Mirror the `Config` stand-in for the
    // second channel: when the author omits `type Params`, synthesize
    // `type Params = ();` and inject a `_params: ()` param. After the config
    // handling above, `config` sits at index 0 (declared or synthesized) and
    // `ctx` at index 1, so inserting `params` at index 1 pushes `ctx` to 2 —
    // giving the `(config, params, ctx)` shape for every declared/omitted
    // combination without special-casing.
    let synthesized_params_type = if params_type.is_some() {
        // User declared `type Params`; trust their init signature.
        None
    } else {
        let synth: syn::ImplItemType = syn::parse_quote!(
            type Params = ();
        );
        let params_param: FnArg = syn::parse_quote!(_params: ());
        init_method_emitted.sig.inputs.insert(1, params_param);
        Some(synth)
    };

    // The SDK no longer prepends subscribe calls to `init` for window
    // streams or lifecycle stages. Pre-#403 those calls fired during
    // `Component::instantiate` — i.e. *before* `try_register_component`
    // published the mailbox — and were refused when the window cap
    // proved the subscriber at receipt through `ctx.resolve_live`.
    // Components subscribe from `wire` (`WindowCapability::subscribe` /
    // `LifecycleCapability::subscribe`) after the trampoline mailbox is
    // registered.
    let wrapped_init = init_method_emitted;
    let dispatch_body = build_dispatch_body(&handlers, fallback.as_ref(), opts.handler_set.as_ref());

    let handler_methods_tokens = handlers.iter().map(|h| &h.method);
    let watch_methods_tokens = watch_handlers.iter().map(|h| &h.method);
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
        opts.handler_set.as_ref().map(|set| (set, &**self_ty)),
        opts.cardinality,
    );

    // ADR-0169: an adopted set's `HandlesKind` markers and `Contract<K>` rows
    // cannot be declared by the set itself (the orphan rule: `Self` precedes
    // the first local type in the trait reference), so they travel through
    // the generated `macro_rules!` bridge the set emits, which this adopter
    // invokes below with the position just past its own rows (ADR-0231 §10).
    // A wasm set's bridge shares the set's name in the macro namespace, so it
    // is reached through the set path this adopter already names.
    let set_markers = opts.handler_set.as_ref().map(|set| {
        let base = position(handlers.len());
        quote! { #set!(#self_ty, #base); }
    });
    let lineage_manifest_consts = build_actor_lineage_manifest_consts(self_ty, opts);
    // ADR-0079 §8: each departure handler's context kind is retained beside
    // the handler kinds, so the host can judge a carried watch context at a
    // republish (ADR-0139 §4).
    let watch_context_kinds: Vec<(Type, Vec<syn::Attribute>)> =
        watch_handlers.iter().map(|h| (watch_context_ty(h), h.cfgs.clone())).collect();
    let kind_retention_statics =
        build_kinds_section_retention_statics(self_ty, &handlers, config_kind_ty, &watch_context_kinds);

    // Issue 525 Phase 4: trait consts (today just NAMESPACE) live
    // on the `Addressable` super-trait, not `Component` / `WasmActor`. Route
    // any const items the user declared inside `#[actor] impl
    // Component for X` to a sibling `impl ::aether_actor::Addressable`
    // block so satisfying `WasmActor: Actor` works without making the
    // user split the impl manually.
    //
    // Validate the const surface first: `NAMESPACE` is required (the
    // marker `impl Addressable` carries it) and is the only authorable const on
    // the `Addressable` super-trait. A removed `SCHEDULING` const (issue 1187)
    // and any stray const are rejected at their own span, and a missing
    // `NAMESPACE` at the type — each a pointed diagnostic rather than a
    // later "no associated const NAMESPACE" error against the surfaceless
    // `Addressable` trait.
    let namespace_expr = validate_addressable_consts(&consts, self_ty, "WasmActor")?;
    let const_tokens = consts.iter();
    // ADR-0241 §5: a guest is named as a native actor is, so its resolver is
    // the native one: `One` (keyless, reached by `ctx.send::<R>(..)` from the
    // root), or `Many` for `#[actor(instanced)]`. Cardinality is derived from
    // the resolver; nothing emits `impl Singleton` here.
    let resolver_ty = if matches!(opts.cardinality, Some(ActorCardinality::Instanced)) {
        quote! { ::aether_actor::Many }
    } else {
        quote! { ::aether_actor::One }
    };
    let watchable = watchable_actor_impl(&quote! { #impl_generics }, &quote! { #self_ty }, &quote! { #where_clause });
    let actor_impl = if consts.is_empty() {
        quote! {}
    } else {
        quote! {
            impl #impl_generics ::aether_actor::Addressable for #self_ty #where_clause {
                #(#const_tokens)*
                type Resolver = #resolver_ty;
            }
            #watchable
        }
    };
    // ADR-0079 §8: one `Watches<W>` per departure handler, naming the context
    // kind its signature fixes, which `ctx.watch` is bounded by.
    let watches_impls = watch_handlers.iter().map(|h| {
        let watched_ty = &h.watched_ty;
        let context_ty = watch_context_ty(h);
        let cfgs = &h.cfgs;
        quote! {
            #(#cfgs)*
            impl #impl_generics ::aether_actor::Watches<#watched_ty> for #self_ty #where_clause {
                type Context = #context_ty;
            }
        }
    });
    // ADR-0241 §5: a guest declared `root` is placed at the root under its
    // published name, as a native root is, so it carries the same permission.
    let root_impl = opts.root.then(|| {
        quote! {
            impl #impl_generics ::aether_actor::Root for #self_ty #where_clause {}
        }
    });
    // ADR-0166 (issue 7210): one `ChildOf<P>` impl per `child_of(..)` entry,
    // each naming `P`'s position in the `Declared::Parents` list, so the list
    // is the actor's whole placement set.
    let child_impls = opts.child_of.iter().enumerate().map(|(index, parent)| {
        let index = position(index);
        quote! {
            impl #impl_generics ::aether_actor::ChildOf<#parent> for #self_ty #where_clause {
                type Index = #index;
            }
        }
    });
    // ADR-0114 addressing amendment: a `child_of(..)` list opens the typed
    // parent door, in the form `root` fixes — infallible for a child-only
    // actor, an `Option` for one that may also be placed at the root. With no
    // `child_of(..)`, no impl is emitted and `ctx.parent()` does not compile.
    let has_parent_impl = (!opts.child_of.is_empty()).then(|| {
        if opts.root {
            quote! {
                impl #impl_generics ::aether_actor::HasParent for #self_ty #where_clause {
                    type Parent<'__aether_parent> = ::core::option::Option<
                        ::aether_actor::InlineParent<'__aether_parent, Self>,
                    >;

                    fn __parent(
                        found: ::core::option::Option<::aether_actor::InlineParent<'_, Self>>,
                    ) -> Self::Parent<'_> {
                        found
                    }
                }
            }
        } else {
            quote! {
                impl #impl_generics ::aether_actor::HasParent for #self_ty #where_clause {
                    type Parent<'__aether_parent> = ::aether_actor::InlineParent<'__aether_parent, Self>;

                    fn __parent(
                        found: ::core::option::Option<::aether_actor::InlineParent<'_, Self>>,
                    ) -> Self::Parent<'_> {
                        found.expect(
                            "a child-only actor has no Root record, so it lives only under a declared parent",
                        )
                    }
                }
            }
        }
    });
    // ADR-0231 §10: the actor's one `Declared` impl lists its `depends(..)`,
    // `spawns(..)`, and `child_of(..)` entries, and each `DependsOn` /
    // `Spawns` / `ChildOf` impl names its entry's position there, so none
    // compiles without its declaration.
    let impl_generics_ts = quote! { #impl_generics };
    let self_ty_ts = quote! { #self_ty };
    let where_clause_ts = quote! { #where_clause };
    let declared = declared_impl(
        &ReplyMarkerSite {
            impl_generics: &impl_generics_ts,
            self_ty: &self_ty_ts,
            where_clause: &where_clause_ts,
            cfgs: &[],
        },
        DeclaredLists { depends: &opts.depends, spawns: &opts.spawns, parents: &opts.child_of },
    );
    // ADR-0230: each `DependsOn<R>` impl names `R`'s position in the
    // `Declared::Depends` list, from which `export!` writes the
    // `InputsRecord::Dependency` records the host checks before `init`.
    let depends_impls = opts.depends.iter().enumerate().map(|(index, target)| {
        let index = position(index);
        quote! {
            impl #impl_generics ::aether_actor::DependsOn<#target> for #self_ty #where_clause {
                type Index = #index;
            }
        }
    });
    // ADR-0114 (issue 6583): one `Spawns<C>` impl per declared inline child,
    // which the typed spawn verbs require, each naming `C`'s position in the
    // `Declared::Spawns` list. Every `export!` that lists this actor requires
    // that list to be listed in its own module (`ListedIn`), so that `export!`
    // must list every declared child. `Placement` projects through the
    // child's `ChildOf<Self>`, so a `spawns(C)` whose `C` does not list this
    // actor in its `child_of(..)` fails here, at the declaration.
    let spawns_impls = opts.spawns.iter().enumerate().map(|(index, child)| {
        let index = position(index);
        quote! {
            impl #impl_generics ::aether_actor::Spawns<#child> for #self_ty #where_clause {
                type Index = #index;
                type Placement = <#child as ::aether_actor::ChildOf<#self_ty>>::Index;
            }
        }
    });
    // ADR-0075: emit one `impl HandlesKind<K> for Self {}` per handler
    // kind. Auto-generated marker impls gate `SendableTo<R>` on the flat
    // typed verbs (`ctx.send::<R>(&k)`) and `Target<K>` on `ActorRef<R>`
    // (`ctx.send_to(&reference, &k)`), so wrong-kind sends are compile
    // errors at the call site. The handler list above
    // is the single source of truth — adding a `#[handler]` automatically
    // updates senders' compile-time checks.
    // ADR-0231 §11: the marker names what the handler requires of its sender,
    // read from its ctx's sender type argument, which the typed sends
    // bound the sending actor against.
    let handles_kind_impls = handlers.iter().map(|h| {
        let impl_generics_ts = quote! { #impl_generics };
        let self_ty_ts = quote! { #self_ty };
        let where_clause_ts = quote! { #where_clause };
        handles_kind_impl(
            &h.kind_ty,
            h.sender.as_ref(),
            &ReplyMarkerSite {
                impl_generics: &impl_generics_ts,
                self_ty: &self_ty_ts,
                where_clause: &where_clause_ts,
                cfgs: &h.cfgs,
            },
        )
    });
    let reply_marker_impls = handlers.iter().map(|h| {
        let impl_generics_ts = quote! { #impl_generics };
        let self_ty_ts = quote! { #self_ty };
        let where_clause_ts = quote! { #where_clause };
        reply_marker_impl(
            h.class,
            &h.reply,
            &h.kind_ty,
            &ReplyMarkerSite {
                impl_generics: &impl_generics_ts,
                self_ty: &self_ty_ts,
                where_clause: &where_clause_ts,
                cfgs: &h.cfgs,
            },
        )
    });

    // ADR-0231 §1 / §4 / §10: one `Contract<K>` row per handler, each at its
    // handler's position in the actor's `Rows` list, and the actor's
    // `CONTRACTS` list, derived from the same row types. An adopted set's rows
    // join `CONTRACTS` through its trait const, and its `Rows` entries end
    // this actor's list through its bridge's `@rows` arm (ADR-0169).
    let contract_rows = handlers.iter().enumerate().map(|(index, h)| {
        contract_row_impl(
            ContractRow { class: h.class, reply: &h.reply, kind_ty: &h.kind_ty, sender: h.sender.as_ref() },
            &position(index),
            &ReplyMarkerSite {
                impl_generics: &impl_generics_ts,
                self_ty: &self_ty_ts,
                where_clause: &where_clause_ts,
                cfgs: &h.cfgs,
            },
        )
    });
    let contract_elements: Vec<TokenStream2> =
        handlers.iter().map(|h| contract_element(h.class, &h.reply, &h.kind_ty, &h.cfgs)).collect();
    let set_contract_rows =
        opts.handler_set.as_ref().map(|set| quote! { <#self_ty as #set>::__AETHER_HANDLER_SET_CONTRACTS });
    let row_specs: Vec<RowSpec<'_>> = handlers
        .iter()
        .map(|h| RowSpec { class: h.class, reply: &h.reply, kind_ty: &h.kind_ty, cfgs: &h.cfgs })
        .collect();
    let rows =
        rows_list(&row_specs, opts.handler_set.as_ref().map_or_else(|| quote! { () }, |set| quote! { #set!(@rows) }))?;
    // A gated set row's slot is an alias the set's bridge defines, pasted
    // beside this actor's own aliases so both live in the `Contracts` impl's
    // anonymous block.
    let set_aliases = opts.handler_set.as_ref().map(|set| quote! { #set!(@aliases); });
    let local_aliases = &rows.aliases;
    let aliases = quote! { #local_aliases #set_aliases };
    let contracts_list = contracts_impl(
        &ReplyMarkerSite {
            impl_generics: &impl_generics_ts,
            self_ty: &self_ty_ts,
            where_clause: &where_clause_ts,
            cfgs: &[],
        },
        &aliases,
        &rows.list,
        &contract_rows_expr(&contract_elements),
        set_contract_rows.as_ref(),
    );

    // ADR-0090: emit the `type Config = …` line in the trait impl —
    // either the user's declaration (passed through) or the macro's
    // synthesized `type Config = ();`.
    let config_type_tokens = match (config_type.as_ref(), synthesized_config_type.as_ref()) {
        (Some(user), _) => quote! { #user },
        (None, Some(synth)) => quote! { #synth },
        (None, None) => unreachable!("synthesized_config_type is Some when user omitted"),
    };

    // ADR-0156 §2: the `type Params = …` line — the user's declaration passed
    // through, or the macro's synthesized `type Params = ();`.
    let params_type_tokens = match (params_type.as_ref(), synthesized_params_type.as_ref()) {
        (Some(user), _) => quote! { #user },
        (None, Some(synth)) => quote! { #synth },
        (None, None) => unreachable!("synthesized_params_type is Some when user omitted"),
    };

    // ADR-0113: when the author declared `type State`, generate the
    // `on_dehydrate` / `on_rehydrate` hooks from the lifted accessors.
    // `Self::State` resolves directly inside `impl WasmActor for Self`.
    // `on_dehydrate` snapshots through `self.dehydrate()` and frames the
    // value with `save_state_kind`; `on_rehydrate` decodes via
    // `PriorState::decode_kind` and either restores through `self.rehydrate`
    // or boots fresh, warning only when bytes were present but did not
    // decode (a reshaped state kind — `K::ID` changed). When `type State`
    // was omitted these are empty and the actor keeps the default no-op
    // hooks (or its own hand-written ones, carried in `lifecycle_methods`).
    let generated_state_hooks = if state_type.is_some() {
        quote! {
            fn on_dehydrate(&mut self, __aether_ctx: &mut ::aether_actor::WasmDropCtx<'_>) {
                let __aether_state = self.dehydrate();
                ::aether_actor::Persistence::save_state_kind::<
                    <Self as ::aether_actor::WasmActor>::Persist,
                >(__aether_ctx, 0, &__aether_state);
            }

            fn on_rehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, Self>,
                __aether_prior: ::aether_actor::PriorState<'_>,
            ) {
                match __aether_prior.decode_kind::<<Self as ::aether_actor::WasmActor>::Persist>() {
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
    // target subtrait `WasmActor`. Route the user's hand-written hooks
    // accordingly — boot hooks into `impl Lifecycle`, hot-swap into
    // `impl WasmActor`. The per-target ctx GATs are pinned to the concrete
    // FFI ctx types here, so a `wire`/`init` body keeps its concrete ctx.
    // `on_rehydrate` rides with the renamed hooks (#6533): its trait method
    // takes the typed ctx, so a hand-written one is renamed and forwarded to
    // like `wire` / `unwire`, and only `on_dehydrate` lands in the trait impl
    // as written.
    let (mut boot_hooks, hotswap_hooks): (Vec<syn::ImplItemFn>, Vec<syn::ImplItemFn>) = lifecycle_methods
        .into_iter()
        .partition(|m| matches!(m.sig.ident.to_string().as_str(), "wire" | "unwire" | "on_rehydrate"));

    // iamacoffeepot/aether#2311: the shared `Lifecycle<S>` `wire`/`unwire`
    // take `(state: &mut S, ctx)`, not a `self` receiver, so a user's
    // `fn wire(&mut self, ctx)` can't satisfy them directly. Mirror the native
    // arm: rename the inherent copies to `__aether_{wire,unwire}` and forward
    // from the trait fn via UFCS (passing the state as the `&mut self`
    // receiver for an un-split `State = Self`). Emitted only when the user
    // provided the hook; the trait's default no-op stands otherwise.
    require_wire_result(&boot_hooks, "ActorInitError")?;
    let (has_wire, has_unwire, has_rehydrate) = rename_lifecycle_hooks(&mut boot_hooks);
    // ADR-0163 §3: `wire` receives the window-bearing `WireCtx`, not a bare
    // `WasmCtx`, so an author can read assets in `wire` but not from a
    // handler (which is handed a `WasmCtx`). The forwarder wraps the
    // `WasmCtx` the lifecycle call builds; `WireCtx` `Deref`s to it, so the
    // user's `wire` body reaches every send / subscribe verb unchanged.
    // Issue 6279: the renamed hooks keep the author's signatures, so read the
    // actor off them. The lifecycle ctx is typed by the actor, and so is every
    // hook that omitted its actor (#6533), so it passes as is; only a hook
    // that spells `Erased` receives the erased view.
    let wire_ctx = boot_hooks.iter().find(|m| m.sig.ident == "__aether_wire").map(|m| erase_ctx_unless_named(&m.sig));
    let unwire_ctx =
        boot_hooks.iter().find(|m| m.sig.ident == "__aether_unwire").map(|m| erase_ctx_unless_named(&m.sig));
    let wire_forward = if has_wire {
        let wire_ctx = wire_ctx.expect("has_wire implies a renamed __aether_wire method");
        quote! {
            fn wire(
                __aether_state: &mut Self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, Self>,
            ) -> ::core::result::Result<(), ::aether_actor::ActorInitError> {
                let mut __aether_wire_ctx = ::aether_actor::WireCtx::__new(#wire_ctx);
                #self_ty::__aether_wire(__aether_state, &mut __aether_wire_ctx)
            }
        }
    } else {
        quote! {}
    };
    let unwire_forward = if has_unwire {
        let unwire_ctx = unwire_ctx.expect("has_unwire implies a renamed __aether_unwire method");
        quote! {
            fn unwire(
                __aether_state: &mut Self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, Self>,
            ) {
                #self_ty::__aether_unwire(__aether_state, #unwire_ctx);
            }
        }
    } else {
        quote! {}
    };
    // ADR-0101 / #6533: `WasmActor::on_rehydrate` takes the typed ctx, so a
    // hand-written hook forwards from it the way `wire` / `unwire` do, erasing
    // only for an override that spells `Erased`.
    let rehydrate_forward = if has_rehydrate {
        let rehydrate_ctx = boot_hooks
            .iter()
            .find(|m| m.sig.ident == "__aether_on_rehydrate")
            .map(|m| erase_ctx_unless_named(&m.sig))
            .expect("has_rehydrate implies a renamed __aether_on_rehydrate method");
        quote! {
            fn on_rehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, Self>,
                __aether_prior: ::aether_actor::PriorState<'_>,
            ) {
                #self_ty::__aether_on_rehydrate(self, #rehydrate_ctx, __aether_prior);
            }
        }
    } else {
        quote! {}
    };

    let export_desc = emit_actor_export_desc(self_ty, namespace_expr);

    Ok(quote! {
        #actor_impl
        #root_impl
        #(#child_impls)*
        #has_parent_impl
        #declared
        #(#depends_impls)*
        #(#spawns_impls)*

        #(#handles_kind_impls)*
        #(#reply_marker_impls)*
        #(#contract_rows)*
        #set_markers
        #contracts_list
        #(#watches_impls)*

        // iamacoffeepot/aether#2311: the boot lifecycle over the runtime state.
        // For an un-split component `State = Self`, so `init` returns `Self` and
        // the `wire`/`unwire` forwarders pass the state as the `&mut self`
        // receiver. The per-target ctx GATs pin the concrete FFI ctx types, the
        // lifecycle ctx typed by the actor.
        impl #impl_generics ::aether_actor::Lifecycle<Self> for #self_ty #where_clause {
            #config_type_tokens
            #params_type_tokens
            type InitError = ::aether_actor::ActorInitError;
            type InitCtx<'__a> = ::aether_actor::WasmInitCtx<'__a>;
            type Ctx<'__a> = ::aether_actor::WasmCtx<'__a, Self>;

            #wrapped_init

            #wire_forward
            #unwire_forward
        }

        // iamacoffeepot/aether#2311: per-kind dispatch over the state, the wasm
        // counterpart of native `Dispatch<S>`. Forwards to the inherent
        // `__aether_dispatch` demux table (`State = Self`, so the state lands
        // as the `&mut self` receiver via UFCS).
        impl #impl_generics ::aether_actor::WasmDispatch<Self> for #self_ty #where_clause {
            fn dispatch(
                __aether_state: &mut Self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Anyone, ::aether_actor::Unchecked>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                #self_ty::__aether_dispatch(__aether_state, __aether_ctx, __aether_mail)
            }
        }

        // The authored block's own rustdoc rides the trait impl that stands in
        // its place, so the actor's headline documentation is a documented item
        // rustdoc renders and link-checks (iamacoffeepot/aether#4848).
        #(#impl_docs)*
        impl #impl_generics #trait_path for #self_ty #where_clause {
            // The runtime state: the identity IS its own runtime (un-split).
            type State = Self;

            #persist_type_tokens

            #(#hotswap_hooks)*
            #rehydrate_forward

            #generated_state_hooks
        }

        impl #impl_generics #self_ty #where_clause {
            #[doc(hidden)]
            pub fn __aether_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Anyone, ::aether_actor::Unchecked>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                #dispatch_body
            }

            #inputs_manifest_consts
            #lineage_manifest_consts

            #(#handler_methods_tokens)*
            #(#watch_methods_tokens)*
            #fallback_method_tokens
            #(#helper_methods_tokens)*
            #(#boot_hooks)*
            #dehydrate_accessor_tokens
            #rehydrate_accessor_tokens
        }

        // ADR-0096: object-safe erasure so a multi-actor module's
        // `export!(public = [A, B, …])` form can hold whichever exported type an
        // instance became in one `Slot<Box<dyn ErasedWasmActor>>` and
        // route the FFI shims through it. Forwards to the inherent
        // dispatch table and the `WasmActor` lifecycle hooks; `init`
        // stays concrete (the `export!` arm tag-matches and boxes).
        impl #impl_generics ::aether_actor::ErasedWasmActor for #self_ty #where_clause {
            fn erased_namespace(&self) -> &'static str {
                <#self_ty as ::aether_actor::Addressable>::NAMESPACE
            }
            fn erased_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Anyone, ::aether_actor::Unchecked>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                self.__aether_dispatch(__aether_ctx, __aether_mail)
            }
            // ADR-0112: the lifecycle ctx is `WasmCtx<'_, Self>` (= Single);
            // upgrade the carried erased ctx to the actor once, where it is
            // born, and downgrade the `Unchecked` view here. `on_rehydrate` takes
            // the same typed ctx (#6533) and upgrades the same way below.
            fn erased_wire(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Anyone, ::aether_actor::Unchecked>,
            ) -> ::core::result::Result<(), ::aether_actor::ActorInitError> {
                <#self_ty as ::aether_actor::Lifecycle<Self>>::wire(self, __aether_ctx.__for_actor::<Self>().as_single())
            }
            fn erased_unwire(&mut self, __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Anyone, ::aether_actor::Unchecked>) {
                <#self_ty as ::aether_actor::Lifecycle<Self>>::unwire(self, __aether_ctx.__for_actor::<Self>().as_single());
            }
            fn erased_on_dehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmDropCtx<'_>,
            ) {
                <#self_ty as ::aether_actor::WasmActor>::on_dehydrate(self, __aether_ctx);
            }
            fn erased_on_rehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Anyone, ::aether_actor::Unchecked>,
                __aether_prior: ::aether_actor::PriorState<'_>,
            ) {
                <#self_ty as ::aether_actor::WasmActor>::on_rehydrate(
                    self,
                    __aether_ctx.__for_actor::<Self>().as_single(),
                    __aether_prior,
                );
            }
        }

        #kind_retention_statics

        #export_desc
    })
}

/// Issue 6279: the ctx expression a dispatch arm calls through —
/// `__aether_ctx.__for_actor::<Self>()` when the callee's signature names its
/// actor, bare `__aether_ctx` otherwise. After `fill_ctx_actor` every handler
/// and `#[fallback]` names its actor except one spelling `Erased`, which alone
/// receives the erased ctx. The guest mirror of native's
/// `erase_unless_ctx_names_actor`: the guest ctx starts erased and upgrades
/// per signature where the native ctx starts typed and downgrades.
fn upgrade_ctx_when_named(sig: &syn::Signature) -> TokenStream2 {
    if ctx_names_actor(sig) {
        quote!(__aether_ctx.__for_actor::<Self>())
    } else {
        quote!(__aether_ctx)
    }
}

/// The ctx expression a `wire` / `unwire` / `on_rehydrate` forwarder hands its
/// hook. The lifecycle ctx is already typed by the actor, so it passes as is
/// when the hook's signature names its actor — every hook but one spelling
/// `Erased`, after `fill_ctx_actor` — and as `__aether_ctx.erase()` otherwise,
/// the same downgrade-only choice native's `erase_unless_ctx_names_actor`
/// makes for every arm.
fn erase_ctx_unless_named(sig: &syn::Signature) -> TokenStream2 {
    if ctx_names_actor(sig) {
        quote!(__aether_ctx)
    } else {
        quote!(__aether_ctx.erase())
    }
}

/// The context kind a departure handler's watches store (ADR-0079 §8): its
/// fourth parameter's type, or the engine's `NoContext` when it has none.
fn watch_context_ty(handler: &WatchHandlerFn) -> Type {
    handler.context_ty.clone().unwrap_or_else(|| syn::parse_quote!(::aether_actor::NoContext))
}

/// Whether a handler's kind spells the engine's departure notice, any path
/// whose last segment is `MonitorNotice`.
fn type_names_monitor_notice(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "MonitorNotice"))
}

/// The one mail handler an actor's departure handlers share (ADR-0079 §8): a
/// silent single handler for the engine's `MonitorNotice`, synthesized so the
/// group gets one `HandlesKind`, one contract row, one manifest record, and
/// one dispatch arm from the code that emits them for every handler.
///
/// Its body asks the host, once per departure handler in declaration order,
/// for the watch through that handler's watched type the notice ends. Each
/// answer takes the watch's stored context as the handler's kind and calls
/// the handler, so one notice runs the handler of each type its sender was
/// watched through once. A handler that spells `Erased` is handed the erased
/// ctx, as a lifecycle hook is.
///
/// It is gated by the disjunction of its handlers' `#[cfg]`s, so it exists in
/// exactly the configurations in which one of them does.
fn departure_handler(watch_handlers: &[WatchHandlerFn]) -> syn::Result<HandlerFn> {
    let calls = watch_handlers.iter().map(|h| {
        let method = &h.method.sig.ident;
        let method_name = method.to_string();
        let watched_ty = &h.watched_ty;
        let context_ty = watch_context_ty(h);
        let ctx = erase_ctx_unless_named(&h.method.sig);
        let cfgs = &h.cfgs;
        let (context_pattern, context_arg) = if h.context_ty.is_some() {
            (quote! { __aether_context }, quote! { , __aether_context })
        } else {
            (quote! { _ }, quote! {})
        };
        quote! {
            #(#cfgs)*
            {
                let __aether_ended = __aether_ctx.__ended_watch::<#watched_ty>().and_then(|__aether_event| {
                    __aether_ctx
                        .__take_watch_context::<#context_ty>(__aether_event.watch, #method_name)
                        .map(|__aether_context| (__aether_event, __aether_context))
                });
                if let ::core::option::Option::Some((__aether_event, #context_pattern)) = __aether_ended {
                    self.#method(#ctx, __aether_event #context_arg);
                }
            }
        }
    });
    let method: syn::ImplItemFn = syn::parse_quote! {
        #[doc(hidden)]
        fn __aether_on_departed(
            &mut self,
            __aether_ctx: &mut ::aether_actor::WasmCtx<'_, Self>,
            _notice: ::aether_actor::__macro_internals::MonitorNotice,
        ) {
            #(#calls)*
        }
    };

    Ok(HandlerFn {
        method,
        kind_ty: syn::parse_quote!(::aether_actor::__macro_internals::MonitorNotice),
        agent_doc: None,
        cfgs: departure_cfgs(watch_handlers)?,
        reply: HandlerReply::None,
        class: HandlerClass::Single,
        unchecked_reason: None,
        response_context: None,
        sender: None,
    })
}

/// The `#[cfg]` of the shared departure handler: nothing when any departure
/// handler is ungated, and otherwise the disjunction of each handler's
/// conjoined predicates.
fn departure_cfgs(watch_handlers: &[WatchHandlerFn]) -> syn::Result<Vec<syn::Attribute>> {
    if watch_handlers.iter().any(|h| h.cfgs.is_empty()) {
        return Ok(Vec::new());
    }
    let predicates =
        watch_handlers.iter().map(|h| conjoined_cfg_predicate(&h.cfgs)).collect::<syn::Result<Vec<_>>>()?;
    Ok(vec![syn::parse_quote!(#[cfg(any(#(#predicates),*))])])
}

/// Issue 552 stage 1: expansion for `#[actor] impl NativeActor for X`
/// — the new native chassis-cap shape. Per-handler ctx + `&self`
/// (Arc-shared) + typed `init`. Mirrors `expand_wasm_actor`'s shape
/// across the wasm/native split.
///
/// Emits, all rooted in the consumer crate's namespace:
///   - `impl Addressable for X` carrying the user-declared `const NAMESPACE`
///     (extracted from the impl block so the `NativeActor: Actor`
///     supertrait bound is satisfied).
///   - `impl HandlesKind<K> for X` per `#[handler]` method — the
///     compile-time gate the flat typed verbs consult through
///     `SendableTo<R>`.
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
/// What an `impl NativeActor for X` expansion emits, selecting between the two
fn build_dispatch_body(
    handlers: &[HandlerFn],
    fallback: Option<&FallbackFn>,
    handler_set: Option<&syn::Path>,
) -> TokenStream2 {
    let arms = handlers.iter().map(|h| {
        let k = &h.kind_ty;
        let method = &h.method.sig.ident;
        // Issue 6279 / #6533: a handler whose signature names its actor — any
        // handler that does not spell `Erased` — is called through the
        // `__for_actor::<Self>()` upgrade, ahead of the per-class downgrade
        // below; one that spells `Erased` receives the erased ctx.
        let ctx = upgrade_ctx_when_named(&h.method.sig);
        // ADR-0112: the dispatch ctx is the full `Unchecked` view. A single
        // handler is called with the downgraded `as_single()` view and the
        // macro auto-replies a `-> R` return through `OutboundReply::reply`
        // on the `Unchecked` ctx; a `-> ()` handler sends nothing. An unchecked
        // handler is called with the `Unchecked` ctx directly and issues its own
        // replies — no auto-reply, regardless of return type. The arm's return
        // code carries its class to the host (#6412): a single arm returns
        // `DISPATCH_HANDLED_RELEASE`, so the substrate frees the dispatch's
        // reply handle, and an unchecked arm returns `DISPATCH_HANDLED`, so the
        // handle it may have kept stays live. ADR-0243 §6: a single
        // `-> Pending<R>` arm accepts the receipt its handler returned,
        // through the `Unchecked` view's `__accept_pending`, and returns
        // `DISPATCH_HANDLED_HOLD`, so the substrate keeps the handle and
        // holds the requester's settlement for the `Held<R>` minted beside
        // the receipt.
        // ADR-0231 §11: an arm whose handler's ctx names a protocol `P` as its
        // sender casts the inbound sender to `P` first and calls the handler
        // with the ctx typed by `P`. A sender the cast refuses never reaches
        // the handler: the helper logs it and answers a request, and the arm
        // returns the code the helper hands back.
        let SenderArm { prelude: prove_sender, ctx } =
            sender_arm(h.sender.as_ref(), k, h.reply.manifest_kind(), &quote! { __aether_refused }, ctx);
        let (call, rc) = match (h.class, &h.reply) {
            (HandlerClass::Single, HandlerReply::Sync(_)) => (
                quote! {
                    #prove_sender
                    let __aether_reply = self.#method(#ctx.as_single(), __aether_decoded);
                    ::aether_actor::OutboundReply::reply(__aether_ctx, &__aether_reply);
                },
                quote! { ::aether_actor::DISPATCH_HANDLED_RELEASE },
            ),
            // ADR-0243 §10: a response arm takes its stored context on the full
            // dispatch ctx and passes it as the fourth argument.
            (HandlerClass::Single, HandlerReply::None) => {
                let rc = quote! { ::aether_actor::DISPATCH_HANDLED_RELEASE };
                let call = silent_call(h.response_context.as_ref(), method, k, &rc, |context| {
                    quote! {
                        #prove_sender
                        self.#method(#ctx.as_single(), __aether_decoded #context);
                    }
                });
                (call, rc)
            }
            (HandlerClass::Single, HandlerReply::Deferred(_)) => (
                quote! {
                    #prove_sender
                    let __aether_pending = self.#method(#ctx.as_single(), __aether_decoded);
                    __aether_ctx.__accept_pending(__aether_pending);
                },
                quote! { ::aether_actor::DISPATCH_HANDLED_HOLD },
            ),
            (HandlerClass::Unchecked, _) => (
                quote! {
                    self.#method(#ctx, __aether_decoded);
                },
                quote! { ::aether_actor::DISPATCH_HANDLED },
            ),
        };
        // `Mail::kind()` and `Kind::ID` are both the typed `KindId`
        // newtype (`KindId: PartialEq`), so they compare directly.
        // iamacoffeepot/aether#4811: the arm rides the handler's own `#[cfg]`s,
        // carried by a statement attribute over the block — the arm names both
        // the kind type and the method, neither of which exists in a
        // configuration that strips the handler.
        let decode_and_call = wasm_arm_body(k, refusal_answer(h.class, &h.reply, k).as_ref(), &call, &rc);
        let cfgs = &h.cfgs;
        quote! {
            #(#cfgs)*
            {
                if __aether_kind == <#k as ::aether_actor::__macro_internals::Kind>::ID {
                    #decode_and_call
                }
            }
        }
    });

    // ADR-0169 §2: after the local chain misses, consult the adopted handler
    // set. Local-first is what makes a locally-declared kind authoritative
    // over an inherited one; the set answers `DISPATCH_UNKNOWN_KIND` when it
    // does not recognize the kind either, leaving the tail below to decide.
    // Any other code passes through unchanged, so the set's arm class reaches
    // the host the way a local arm's does (#6412). The set's dispatch method
    // takes the ctx typed by its adopter (#6533), so the erased dispatch ctx
    // upgrades once here, as a handler arm that names its actor does. The set
    // borrows the mail, so a miss leaves it for the tail below (#6569).
    let set_delegation = handler_set.map(|set| {
        quote! {
            let __aether_set_rc = <Self as #set>::__aether_handler_set_dispatch(
                self,
                __aether_ctx.__for_actor::<Self>(),
                &__aether_mail,
            );
            if __aether_set_rc != ::aether_actor::DISPATCH_UNKNOWN_KIND {
                return __aether_set_rc;
            }
        }
    });

    let tail = if let Some(f) = fallback {
        let method = &f.method.sig.ident;
        // Issue 6279 / #6533: a `#[fallback]` whose signature names its actor
        // — any that does not spell `Erased` — is called through the
        // `__for_actor::<Self>()` upgrade, like a handler.
        let ctx = upgrade_ctx_when_named(&f.method.sig);
        // ADR-0112: a `#[fallback]` keeps its `WasmCtx<'_>` (= Single)
        // signature; the dispatch ctx is `Unchecked`, so downgrade.
        quote! {
            self.#method(#ctx.as_single(), __aether_mail);
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
        #set_delegation
        #tail
    }
}

/// One guest arm's decode, `call`, and `return rc`, shared by the `#[actor]`
/// and `#[handler_set]` expansions. A recognized kind id whose payload fails
/// to decode falls through to the tail (the `#[fallback]`, else
/// `DISPATCH_UNKNOWN_KIND`) rather than reporting HANDLED for a handler that
/// never ran (iamacoffeepot/aether#2455). ADR-0231 §3: a replying row passes
/// the `answer` a refused typed path goes through, and answers it with its
/// reply's `From<PathRefused>`, which frees the dispatch's reply handle.
pub fn wasm_arm_body(
    kind_ty: &Type,
    answer: Option<&TokenStream2>,
    call: &TokenStream2,
    rc: &TokenStream2,
) -> TokenStream2 {
    let Some(answer) = answer else {
        return quote! {
            if let ::core::option::Option::Some(__aether_decoded) = __aether_mail.decode_kind::<#kind_ty>() {
                #call
                return #rc;
            }
        };
    };
    quote! {
        match __aether_mail.__decode_kind_or_refused::<#kind_ty>() {
            ::core::result::Result::Ok(__aether_decoded) => {
                #call
                return #rc;
            }
            ::core::result::Result::Err(__aether_error) => {
                if let ::core::option::Option::Some(__aether_refused) =
                    ::aether_actor::__macro_internals::refused_reply(__aether_error.as_ref(), #answer)
                {
                    ::aether_actor::OutboundReply::reply(__aether_ctx, &__aether_refused);
                    return ::aether_actor::DISPATCH_HANDLED_RELEASE;
                }
            }
        }
    }
}
