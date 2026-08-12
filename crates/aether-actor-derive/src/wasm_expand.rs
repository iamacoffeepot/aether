use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{FnArg, ImplItem, ItemImpl, Type};

use crate::diagnostics::extract_agent_doc;
use crate::handler_parse::{
    FallbackFn, HandlerClass, HandlerFn, HandlerReply, HandlerVariant, attr_is_fallback, attr_is_handler,
    classify_handler_reply, extract_handler_kind_type, handler_cfgs, multi_kind_or_return_error, parse_handler_class,
    parse_handler_variant, reject_duplicate_handler_kinds, rename_lifecycle_hooks, validate_addressable_consts,
    validate_fallback_sig,
};
use crate::manifest::{
    build_actor_lineage_manifest_consts, build_inputs_manifest_consts, build_kinds_section_retention_statics,
};
use crate::opts::{ActorCardinality, ActorOpts};

/// Wasm-actor expansion — `#[actor] impl WasmActor for X` (or
/// the back-compat `impl Component for X`). Emits the full wasm
/// surface: dispatch table referencing `aether_actor::WasmCtx<'_>`,
/// init wrapper, `aether.kinds.inputs` manifest consts, kind retention
/// statics, plus the `HandlesKind<K>` and `Addressable` impls common to both
/// shapes.
#[allow(clippy::too_many_lines)] // emits the full wasm-actor surface in one go
pub fn expand_wasm_actor(item: ItemImpl, opts: &ActorOpts) -> syn::Result<TokenStream2> {
    let self_ty = &item.self_ty;
    if opts.composable && !matches!(opts.cardinality, Some(ActorCardinality::Instanced)) {
        return Err(syn::Error::new_spanned(
            self_ty,
            "`composable` requires explicit instanced Wasm cardinality; use `#[actor(instanced, composable)]`",
        ));
    }
    if opts.root {
        return Err(syn::Error::new_spanned(
            self_ty,
            "`root` is unavailable to Wasm actors; loaded modules are embedded entries, not actor-tree roots (ADR-0166)",
        ));
    }
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

    let mut init_method: Option<syn::ImplItemFn> = None;
    let mut lifecycle_methods: Vec<syn::ImplItemFn> = Vec::new();
    let mut handlers: Vec<HandlerFn> = Vec::new();
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
                    let variant = parse_handler_variant(&f.attrs[idx])?;
                    if variant == HandlerVariant::Task {
                        return Err(syn::Error::new_spanned(
                            &f,
                            "dispatch completions are native-only (ADR-0093 §7); \
                             `#[handler(task)]` is not supported in wasm components",
                        ));
                    }
                    let kind_ty = extract_handler_kind_type(&f.sig)?;
                    let agent_doc = extract_agent_doc(&f.attrs);
                    let reply = classify_handler_reply(&f.sig.output);
                    // ADR-0112 / ADR-0134: read the reply class off the marker
                    // path.
                    let class = parse_handler_class(&f.attrs[idx], variant)?;
                    // ADR-0134: a multi handler emits through `ctx.emit` and
                    // must return `()` (the emissions are the reply, not a
                    // return value); `K` rides its `Multi<K>` ctx marker.
                    let multi_kind = multi_kind_or_return_error(class, &reply, &f.sig)?;
                    // iamacoffeepot/aether#4811: the method keeps its own `#[cfg]`s
                    // (only the marker attribute is removed), so clone them for
                    // the artifacts derived from it.
                    let cfgs = handler_cfgs(&f.attrs);
                    f.attrs.remove(idx);
                    handlers.push(HandlerFn { method: f, kind_ty, agent_doc, reply, class, multi_kind, cfgs });
                } else if let Some(idx) = fallback_attr_idx {
                    if fallback.is_some() {
                        return Err(syn::Error::new_spanned(&f, "at most one #[fallback] method per component"));
                    }
                    validate_fallback_sig(&f.sig)?;
                    let agent_doc = extract_agent_doc(&f.attrs);
                    f.attrs.remove(idx);
                    fallback = Some(FallbackFn { method: f, agent_doc });
                } else if name == "init" {
                    init_method = Some(f);
                } else if matches!(name.as_str(), "wire" | "unwire" | "on_dehydrate" | "on_rehydrate") {
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
    let dispatch_body = build_dispatch_body(&handlers, fallback.as_ref(), opts.handler_set.as_ref());

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
        opts.handler_set.as_ref().map(|set| (set, &**self_ty)),
    );

    // ADR-0169 leaves an adopted set's `HandlesKind` markers unemitted on this
    // path. The orphan rule forbids the set declaring them itself (`Self`
    // precedes the first local type in the trait reference), so they travel
    // through a generated `macro_rules!` bridge — which a native set emits and
    // its adopters invoke. A wasm set does not, because nothing on this
    // transport reads the marker: the widget family addresses its members
    // parent-to-child by name through `RelativeMailbox::send<K: Kind>`, which
    // carries no `HandlesKind` bound. The scoped consequence is that a wasm
    // set's kinds are not sendable through the typed resolver
    // (`ctx.actor::<R>().send(&k)`).
    let lineage_manifest_consts = build_actor_lineage_manifest_consts(self_ty, opts);
    let kind_retention_statics = build_kinds_section_retention_statics(self_ty, &handlers, config_kind_ty);

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
    validate_addressable_consts(&consts, self_ty, "WasmActor")?;
    let const_tokens = consts.iter();
    // ADR-0119: an FFI/wasm component is embedded — it resolves under the
    // reserved `aether.embedded` scope. Default `Embedded` (keyless ⇒
    // `Singleton`, reached by `ctx.actor::<R>()`); `#[actor(instanced)]`
    // selects `EmbeddedMany` for a spawn-sibling child (ADR-0097). Cardinality
    // is derived from the resolver; nothing emits `impl Singleton` here.
    let resolver_ty = if matches!(opts.cardinality, Some(ActorCardinality::Instanced)) {
        quote! { ::aether_actor::EmbeddedMany }
    } else {
        quote! { ::aether_actor::Embedded }
    };
    let actor_impl = if consts.is_empty() {
        quote! {}
    } else {
        quote! {
            impl #impl_generics ::aether_actor::Addressable for #self_ty #where_clause {
                #(#const_tokens)*
                type Resolver = #resolver_ty;
            }
        }
    };
    let root_impl = opts.root.then(|| {
        quote! {
            impl #impl_generics ::aether_actor::Root for #self_ty #where_clause {}
        }
    });
    let module_child_impl = opts.composable.then(|| {
        quote! {
            impl #impl_generics ::aether_actor::ModuleChild for #self_ty #where_clause {}
        }
    });
    let child_impls = opts.child_of.iter().map(|parent| {
        quote! {
            impl #impl_generics ::aether_actor::ChildOf<#parent>
                for #self_ty #where_clause {}
        }
    });

    // ADR-0075: emit one `impl HandlesKind<K> for Self {}` per handler
    // kind. Auto-generated marker impls gate
    // `ActorMailbox<'_, R, T>::send::<K>` (constructed via
    // `ctx.actor::<R>()` / `ctx.resolve_actor::<R>(name)`) so wrong-kind
    // sends are compile errors at the call site. The handler list above
    // is the single source of truth — adding a `#[handler]` automatically
    // updates senders' compile-time checks.
    let handles_kind_impls = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        let cfgs = &h.cfgs;
        quote! {
            #(#cfgs)*
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
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_>,
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
    let (mut boot_hooks, hotswap_hooks): (Vec<syn::ImplItemFn>, Vec<syn::ImplItemFn>) =
        lifecycle_methods.into_iter().partition(|m| matches!(m.sig.ident.to_string().as_str(), "wire" | "unwire"));

    // iamacoffeepot/aether#2311: the shared `Lifecycle<S>` `wire`/`unwire`
    // take `(state: &mut S, ctx)`, not a `self` receiver, so a user's
    // `fn wire(&mut self, ctx)` can't satisfy them directly. Mirror the native
    // arm: rename the inherent copies to `__aether_{wire,unwire}` and forward
    // from the trait fn via UFCS (passing the state as the `&mut self`
    // receiver for an un-split `State = Self`). Emitted only when the user
    // provided the hook; the trait's default no-op stands otherwise.
    let (has_wire, has_unwire) = rename_lifecycle_hooks(&mut boot_hooks);
    // ADR-0163 §3: `wire` receives the window-bearing `WireCtx`, not a bare
    // `WasmCtx`, so an author can read assets in `wire` but not from a
    // handler (which is handed a `WasmCtx`). The forwarder wraps the
    // `WasmCtx` the lifecycle call builds; `WireCtx` `Deref`s to it, so the
    // user's `wire` body reaches every send / subscribe verb unchanged.
    let wire_forward = if has_wire {
        quote! {
            fn wire(
                __aether_state: &mut Self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_>,
            ) {
                let mut __aether_wire_ctx = ::aether_actor::WireCtx::__new(__aether_ctx);
                #self_ty::__aether_wire(__aether_state, &mut __aether_wire_ctx);
            }
        }
    } else {
        quote! {}
    };
    let unwire_forward = if has_unwire {
        quote! {
            fn unwire(
                __aether_state: &mut Self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_>,
            ) {
                #self_ty::__aether_unwire(__aether_state, __aether_ctx);
            }
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        #actor_impl
        #root_impl
        #module_child_impl
        #(#child_impls)*

        #(#handles_kind_impls)*

        // iamacoffeepot/aether#2311: the boot lifecycle over the runtime state.
        // For an un-split component `State = Self`, so `init` returns `Self` and
        // the `wire`/`unwire` forwarders pass the state as the `&mut self`
        // receiver. The per-target ctx GATs pin the concrete FFI ctx types.
        impl #impl_generics ::aether_actor::Lifecycle<Self> for #self_ty #where_clause {
            #config_type_tokens
            #params_type_tokens
            type InitError = ::aether_actor::ActorInitError;
            type InitCtx<'__a> = ::aether_actor::WasmInitCtx<'__a>;
            type Ctx<'__a> = ::aether_actor::WasmCtx<'__a>;

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
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                #self_ty::__aether_dispatch(__aether_state, __aether_ctx, __aether_mail)
            }
        }

        impl #impl_generics #trait_path for #self_ty #where_clause {
            // The runtime state: the identity IS its own runtime (un-split).
            type State = Self;

            #persist_type_tokens

            #(#hotswap_hooks)*

            #generated_state_hooks
        }

        impl #impl_generics #self_ty #where_clause {
            #[doc(hidden)]
            pub fn __aether_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                #dispatch_body
            }

            #inputs_manifest_consts
            #lineage_manifest_consts

            #(#handler_methods_tokens)*
            #fallback_method_tokens
            #(#helper_methods_tokens)*
            #(#boot_hooks)*
            #dehydrate_accessor_tokens
            #rehydrate_accessor_tokens
        }

        // ADR-0096: object-safe erasure so a multi-actor module's
        // `export!(A, B, …)` arm can hold whichever exported type an
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
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                __aether_mail: ::aether_actor::Mail<'_>,
            ) -> u32 {
                self.__aether_dispatch(__aether_ctx, __aether_mail)
            }
            // ADR-0112: the lifecycle hooks keep their `WasmCtx<'_>` (= Single)
            // default signatures; downgrade the carried `Manual` ctx here.
            fn erased_wire(&mut self, __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>) {
                <#self_ty as ::aether_actor::Lifecycle<Self>>::wire(self, __aether_ctx.as_single());
            }
            fn erased_unwire(&mut self, __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>) {
                <#self_ty as ::aether_actor::Lifecycle<Self>>::unwire(self, __aether_ctx.as_single());
            }
            fn erased_on_dehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmDropCtx<'_>,
            ) {
                <#self_ty as ::aether_actor::WasmActor>::on_dehydrate(self, __aether_ctx);
            }
            fn erased_on_rehydrate(
                &mut self,
                __aether_ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                __aether_prior: ::aether_actor::PriorState<'_>,
            ) {
                <#self_ty as ::aether_actor::WasmActor>::on_rehydrate(self, __aether_ctx.as_single(), __aether_prior);
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
///   - `impl Addressable for X` carrying the user-declared `const NAMESPACE`
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
/// What an `impl NativeActor for X` expansion emits, selecting between the two
fn build_dispatch_body(
    handlers: &[HandlerFn],
    fallback: Option<&FallbackFn>,
    handler_set: Option<&syn::Path>,
) -> TokenStream2 {
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
            // ADR-0134: a multi handler is called with the `Multi<K>` view
            // (`K` inferred from its ctx signature); it emits 0..n mails and
            // returns `()`, so there is no auto-reply.
            (HandlerClass::Multi, _) => quote! {
                self.#method(__aether_ctx.as_multi(), __aether_decoded);
            },
        };
        // `Mail::kind()` and `Kind::ID` are both the typed `KindId`
        // newtype (`KindId: PartialEq`), so they compare directly.
        // iamacoffeepot/aether#4811: the arm rides the handler's own `#[cfg]`s,
        // carried by a statement attribute over the block — the arm names both
        // the kind type and the method, neither of which exists in a
        // configuration that strips the handler.
        let cfgs = &h.cfgs;
        quote! {
            #(#cfgs)*
            {
                if __aether_kind == <#k as ::aether_actor::__macro_internals::Kind>::ID {
                    if let ::core::option::Option::Some(__aether_decoded) =
                        __aether_mail.decode_kind::<#k>()
                    {
                        #call
                        return ::aether_actor::DISPATCH_HANDLED;
                    }
                    // A recognized kind id whose payload fails to decode falls
                    // through to the tail (the `#[fallback]`, else
                    // `DISPATCH_UNKNOWN_KIND`), mirroring the native arm's
                    // `return Option::None` rather than reporting HANDLED for a
                    // handler that never ran (iamacoffeepot/aether#2455). No later
                    // arm matches, since the id already matched this one.
                }
            }
        }
    });

    // ADR-0169 §2: after the local chain misses, consult the adopted handler
    // set. Local-first is what makes a locally-declared kind authoritative
    // over an inherited one; the set answers `DISPATCH_UNKNOWN_KIND` when it
    // does not recognize the kind either, leaving the tail below to decide.
    let set_delegation = handler_set.map(|set| {
        quote! {
            if <Self as #set>::__aether_handler_set_dispatch(self, __aether_ctx, __aether_mail)
                == ::aether_actor::DISPATCH_HANDLED
            {
                return ::aether_actor::DISPATCH_HANDLED;
            }
        }
    });

    let tail = if let Some(f) = fallback {
        let method = &f.method.sig.ident;
        // ADR-0112: a `#[fallback]` keeps its `WasmCtx<'_>` (= Single)
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
        #set_delegation
        #tail
    }
}
