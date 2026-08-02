//! ADR-0169: `#[handler_set]` — a reusable block of handlers hosted on a
//! trait, adopted by an actor through `#[actor(handler_set(T))]`.
//!
//! The trait's `#[handler::<class>]` methods carry the shared behavior as
//! default bodies; its required methods are the accessors those bodies reach
//! through. The expansion emits the trait with the handler attributes stripped
//! plus two hidden items an adopter's `#[actor]` expansion consumes:
//!
//! - `__aether_handler_set_dispatch` — the set's own kind-id if-chain,
//!   returning `DISPATCH_HANDLED` or `DISPATCH_UNKNOWN_KIND`. The adopter
//!   calls it after its local chain misses (ADR-0169 §2), which is what makes
//!   a locally-declared handler authoritative over an inherited one.
//! - `__AETHER_HANDLER_SET_MANIFEST` (wasm sets) — the set's
//!   `aether.kinds.inputs` record bytes, in the same encoding
//!   `build_inputs_manifest_consts` emits, so an adopter's manifest reports its
//!   full receive surface (ADR-0033). Typed `&'static [u8]` rather than
//!   `[u8; N]`: an associated const whose *type* mentions `Self::LEN` is not
//!   expressible on a trait, and `<[u8]>::len` is const, so the adopter
//!   recovers the length for its own array arithmetic.
//! - `__aether_handler_set_capabilities` (native sets) — the set's
//!   `HandlerCapability` rows, which an adopter splices into its own
//!   `Dispatch::capabilities` / `measured_kinds` so the native describe surface
//!   and the cost table keep naming every kind the actor really receives
//!   (ADR-0109 §5). It is the native counterpart of the wasm manifest const:
//!   the native path carries its receive surface as link-time inventory and
//!   runtime rows rather than as custom-section bytes.
//!
//! # Markers
//!
//! A native set additionally emits a `#[macro_export] macro_rules!` bridge that
//! pastes the set's `impl HandlesKind<K> for $ty {}` markers and the matching
//! `HandlerEntry` inventory rows. The bridge exists because the orphan rule
//! forecloses the set declaring the markers itself — `impl<T: Set>
//! HandlesKind<K> for T` puts the `Self` type parameter ahead of the first
//! local type in the trait reference. The adopter's `#[actor]` emits one
//! invocation of it.
//!
//! Two properties of that bridge are load-bearing. It is invoked **unqualified**
//! rather than as `crate::__aether_handler_set_markers_…!` — a macro-expanded
//! `#[macro_export]` macro named by absolute path from inside its own crate
//! trips `macro_expanded_macro_exports_accessed_by_absolute_paths` (rust-lang
//! issue 52234), while the unqualified form resolves through the crate-root
//! macro prelude and is order-independent, so an adopter above the set's own
//! `mod` line still sees it. And a `macro_rules!` pastes paths at the use site,
//! so a native set's kind types need spellings that resolve from every adopter.
//!
//! A wasm set emits no bridge. Its adopters — the widget family — address each
//! other by name through `RelativeMailbox::send<K: Kind>`, which carries no
//! `HandlesKind` bound, so a marker there would gate nothing.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{FnArg, ItemTrait, TraitItem, Type};

use crate::diagnostics::extract_agent_doc;
use crate::handler_parse::{
    HandlerClass, HandlerFn, HandlerReply, HandlerVariant, attr_is_fallback, attr_is_handler, classify_handler_reply,
    extract_handler_kind_type, extract_native_actor_handler_kind, multi_kind_or_return_error, parse_handler_class,
    parse_handler_variant, reject_duplicate_handler_kinds,
};
use crate::manifest::build_handler_set_manifest_const;

/// Which actor transport a set's handlers are written against, read off the
/// ctx parameter's type name the same way `expand_handlers` reads the trait
/// name. A set is entirely one or the other: the ctx type, the receiver shape,
/// and the dispatch signature all follow from it, and an adopter on the other
/// transport fails to unify at the delegation call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SetTransport {
    Wasm,
    Native,
}

impl SetTransport {
    /// The ctx type the set's dispatch method takes, in its `Manual` view —
    /// the same view `#[actor]` dispatches with, so the adopter can hand its
    /// own ctx straight through.
    fn dispatch_ctx(self) -> TokenStream2 {
        match self {
            Self::Wasm => quote! { ::aether_actor::WasmCtx<'_, ::aether_actor::Manual> },
            Self::Native => quote! { ::aether_substrate::actor::native::NativeCtx<'_, ::aether_actor::Manual> },
        }
    }

    /// The inbound-mail parameters the dispatch method takes, in the shape the
    /// adopter's own dispatch seam already holds them: wasm hands the whole
    /// `Mail<'_>` through, native has already split the envelope into a kind id
    /// and a payload slice by the time `Dispatch::dispatch` runs.
    fn mail_params(self) -> TokenStream2 {
        match self {
            Self::Wasm => quote! { __aether_mail: ::aether_actor::Mail<'_> },
            Self::Native => quote! {
                __aether_kind: ::aether_substrate::mail::KindId,
                __aether_payload: &[u8],
            },
        }
    }

    /// The statement that binds `__aether_kind` before the if-chain reads it.
    /// Native receives it as a parameter, so only wasm has one to emit.
    fn kind_binding(self) -> TokenStream2 {
        match self {
            Self::Wasm => quote! { let __aether_kind = __aether_mail.kind(); },
            Self::Native => quote! {},
        }
    }

    /// One arm's kind-id test and payload decode, spelled the way the matching
    /// `#[actor]` expansion spells them on that transport.
    fn arm_terms(self, kind_ty: &Type) -> (TokenStream2, TokenStream2) {
        match self {
            Self::Wasm => (
                quote! { __aether_kind == <#kind_ty as ::aether_actor::__macro_internals::Kind>::ID },
                quote! { __aether_mail.decode_kind::<#kind_ty>() },
            ),
            Self::Native => (
                quote! { __aether_kind.0 == <#kind_ty as ::aether_data::Kind>::ID.0 },
                quote! { <#kind_ty as ::aether_data::Kind>::decode_from_bytes(__aether_payload) },
            ),
        }
    }
}

/// Read the transport off a handler's ctx parameter (the second one). A ctx
/// whose type name is neither `WasmCtx` nor `NativeCtx` earns a pointed error
/// rather than an opaque failure inside the emitted dispatch chain.
fn transport_of(sig: &syn::Signature) -> syn::Result<SetTransport> {
    fn shape_err<T: quote::ToTokens>(span: T) -> syn::Error {
        syn::Error::new_spanned(
            span,
            "a #[handler_set] handler's second parameter must be a ctx reference — \
             `&mut WasmCtx<'_>` for a wasm set or `&mut NativeCtx<'_>` for a native one (ADR-0169)",
        )
    }
    let param = sig.inputs.get(1).ok_or_else(|| shape_err(sig))?;
    let FnArg::Typed(pt) = param else {
        return Err(shape_err(param));
    };
    let Type::Reference(ctx_ref) = &*pt.ty else {
        return Err(shape_err(&pt.ty));
    };
    let Type::Path(ctx_path) = &*ctx_ref.elem else {
        return Err(shape_err(&pt.ty));
    };
    let last = ctx_path.path.segments.last().ok_or_else(|| shape_err(&pt.ty))?;
    match last.ident.to_string().as_str() {
        "WasmCtx" => Ok(SetTransport::Wasm),
        "NativeCtx" => Ok(SetTransport::Native),
        _ => Err(shape_err(&pt.ty)),
    }
}

/// Whether a set's handlers take the split `state: &mut Self::State` first
/// parameter rather than a `self` receiver (ADR-0169 §5). Read off the first
/// handler; the rest are checked against it so a set cannot mix shapes.
fn is_split_shape(sig: &syn::Signature) -> bool {
    matches!(sig.inputs.first(), Some(FnArg::Typed(_)))
}

/// Expand `#[handler_set] trait T { … }`.
#[allow(clippy::too_many_lines)] // collects, validates, and emits the set in one pass
pub fn expand_handler_set(mut item: ItemTrait) -> syn::Result<TokenStream2> {
    let mut handlers: Vec<HandlerFn> = Vec::new();
    let mut transport: Option<SetTransport> = None;
    let mut split: Option<bool> = None;

    for trait_item in &mut item.items {
        let TraitItem::Fn(f) = trait_item else {
            continue;
        };
        if f.attrs.iter().any(attr_is_fallback) {
            return Err(syn::Error::new_spanned(
                &*f,
                "#[fallback] belongs to the adopting actor, not to a #[handler_set] — a set \
                 declares the kinds it owns, and the catch-all tail stays with the actor (ADR-0169)",
            ));
        }
        let Some(idx) = f.attrs.iter().position(attr_is_handler) else {
            continue;
        };

        let variant = parse_handler_variant(&f.attrs[idx])?;
        if variant == HandlerVariant::Task {
            return Err(syn::Error::new_spanned(
                &*f,
                "#[handler(task)] is not supported in a #[handler_set] — a dispatch \
                 completion is routed by output type rather than kind id, so it cannot \
                 ride the set's kind-id chain (ADR-0169)",
            ));
        }
        let Some(body) = f.default.as_ref() else {
            return Err(syn::Error::new_spanned(
                &*f,
                "a #[handler_set] handler needs a default body — the shared behavior is \
                 what the set carries. Declare it as a required method only if every \
                 adopter implements it, in which case it does not belong in the set (ADR-0169)",
            ));
        };
        let _ = body;

        let this_transport = transport_of(&f.sig)?;
        if *transport.get_or_insert(this_transport) != this_transport {
            return Err(syn::Error::new_spanned(
                &f.sig,
                "a #[handler_set] is wasm or native throughout — this handler's ctx type \
                 disagrees with an earlier one in the same set (ADR-0169 §5)",
            ));
        }
        let this_split = is_split_shape(&f.sig);
        if *split.get_or_insert(this_split) != this_split {
            return Err(syn::Error::new_spanned(
                &f.sig,
                "a #[handler_set] uses one authoring shape throughout — mixing a `self` \
                 receiver with the split `state: &mut Self::State` first parameter would \
                 emit a dispatch chain that cannot call both (ADR-0169 §5)",
            ));
        }

        let kind_ty = match this_transport {
            SetTransport::Wasm => extract_handler_kind_type(&f.sig)?,
            SetTransport::Native => {
                let (kind_ty, is_slice) = extract_native_actor_handler_kind(&f.sig, this_split)?;
                if is_slice {
                    return Err(syn::Error::new_spanned(
                        &f.sig,
                        "a batched `mail: &[K]` handler is not supported in a #[handler_set] — \
                         the set's dispatch chain decodes one value per arm, so the slice \
                         handler belongs in the adopting actor's own block (ADR-0169)",
                    ));
                }
                kind_ty
            }
        };
        let agent_doc = extract_agent_doc(&f.attrs);
        let reply = classify_handler_reply(&f.sig.output);
        let class = parse_handler_class(&f.attrs[idx], variant)?;
        let multi_kind = multi_kind_or_return_error(class, &reply, &f.sig)?;
        f.attrs.remove(idx);

        // The dispatch chain only needs the signature; the body stays on the
        // trait as the method's default. `HandlerFn` carries a full
        // `ImplItemFn`, so rebuild the shell from the trait method's parts —
        // its ident is what the emitted arm calls.
        let method = syn::ImplItemFn {
            attrs: Vec::new(),
            vis: syn::Visibility::Inherited,
            defaultness: None,
            sig: f.sig.clone(),
            block: syn::parse_quote!({}),
        };
        handlers.push(HandlerFn { method, kind_ty, agent_doc, reply, class, multi_kind });
    }

    if handlers.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "#[handler_set] requires at least one #[handler] method with a default body — \
             a set with no handlers is a plain trait (ADR-0169)",
        ));
    }
    reject_duplicate_handler_kinds(&handlers)?;

    let transport = transport.expect("handlers is non-empty, so a transport was recorded");
    let split = split.expect("handlers is non-empty, so a shape was recorded");
    let dispatch_ctx = transport.dispatch_ctx();
    let mail_params = transport.mail_params();
    let dispatch_body = build_set_dispatch_body(&handlers, transport, split);

    // The dispatch method is a provided method on the trait, so an adopter
    // reaches it as `<Self as T>::__aether_handler_set_dispatch`. Split sets
    // take the state explicitly; un-split ones take the `&mut self` receiver,
    // matching the shape their handlers were written in.
    let dispatch_receiver = if split {
        quote! { __aether_state: &mut Self::State }
    } else {
        quote! { &mut self }
    };
    let state_assoc = if split {
        quote! {
            /// The runtime state this set's handlers act on, pinned by the
            /// adopting actor's own `type State`.
            type State;
        }
    } else {
        quote! {}
    };

    item.items.push(syn::parse_quote! {
        #[doc(hidden)]
        fn __aether_handler_set_dispatch(
            #dispatch_receiver,
            __aether_ctx: &mut #dispatch_ctx,
            #mail_params
        ) -> u32 {
            #dispatch_body
        }
    });
    // Each transport carries its receive surface differently: wasm as
    // `aether.kinds.inputs` section bytes, native as runtime capability rows
    // plus the link-time markers the bridge below pastes. Emitting only the one
    // that transport reads keeps a set from carrying an item no adopter of it
    // can consume.
    match transport {
        SetTransport::Wasm => {
            let manifest_const = build_handler_set_manifest_const(&handlers);
            item.items.push(syn::parse_quote! {
                #[doc(hidden)]
                const __AETHER_HANDLER_SET_MANIFEST: &'static [u8] = #manifest_const;
            });
        }
        SetTransport::Native => {
            let capability_rows = build_native_capability_rows(&handlers);
            item.items.push(syn::parse_quote! {
                #[doc(hidden)]
                fn __aether_handler_set_capabilities()
                    -> ::std::vec::Vec<::aether_substrate::actor::native::HandlerCapability>
                {
                    #capability_rows
                }
            });
        }
    }
    if !state_assoc.is_empty() {
        item.items.push(syn::parse_quote! { #state_assoc });
    }

    let marker_bridge = match transport {
        SetTransport::Wasm => quote! {},
        SetTransport::Native => build_native_marker_bridge(&item.ident, &handlers),
    };

    Ok(quote! {
        #item
        #marker_bridge
    })
}

/// The set's `HandlerCapability` rows, in the same shape `#[actor]` builds for
/// an actor's own handlers. An adopter appends these to its `capabilities()`
/// and reads their ids for `measured_kinds()`, so the native describe surface
/// and the per-handler cost table name the inherited kinds too.
fn build_native_capability_rows(handlers: &[HandlerFn]) -> TokenStream2 {
    let rows = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        quote! {
            ::aether_substrate::actor::native::HandlerCapability {
                id: <#kind_ty as ::aether_data::Kind>::ID,
                name: <#kind_ty as ::aether_data::Kind>::NAME.to_owned(),
                doc: ::core::option::Option::None,
                reply: ::aether_data::ReplyContract::None,
            }
        }
    });
    quote! { ::std::vec![#(#rows),*] }
}

/// The `macro_rules!` bridge carrying the set's `HandlesKind` markers and
/// `HandlerEntry` inventory rows to an adopter (see the module header). The
/// orphan rule forbids the set impl'ing `HandlesKind` for its own adopters, and
/// a macro is the only route that pastes the impls into the adopter's crate.
///
/// The name is derived from the trait ident, so two sets sharing an ident in
/// one crate collide — the same collision `#[macro_export]` itself would raise.
fn build_native_marker_bridge(set_ident: &syn::Ident, handlers: &[HandlerFn]) -> TokenStream2 {
    let macro_ident = format_ident!("__aether_handler_set_markers_{}", set_ident);
    let markers = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        quote! {
            impl ::aether_actor::HandlesKind<#kind_ty> for $ty {}
        }
    });
    let inventory = handlers.iter().map(|h| {
        let kind_ty = &h.kind_ty;
        let reply_expr = if let Some(reply_ty) = h.reply.manifest_kind() {
            quote! { ::core::option::Option::Some(<#reply_ty as ::aether_data::Kind>::ID) }
        } else {
            quote! { ::core::option::Option::None }
        };
        quote! {
            #[cfg(not(target_family = "wasm"))]
            ::aether_data::name_inventory::inventory::submit! {
                ::aether_data::name_inventory::HandlerEntry {
                    namespace: <$ty as ::aether_actor::Addressable>::NAMESPACE,
                    id: <#kind_ty as ::aether_data::Kind>::ID,
                    name: <#kind_ty as ::aether_data::Kind>::NAME,
                    reply: #reply_expr,
                }
            }
        }
    });
    quote! {
        #[macro_export]
        #[doc(hidden)]
        macro_rules! #macro_ident {
            ($ty:ty) => {
                #(#markers)*
                #(#inventory)*
            };
        }
    }
}

/// The set's own kind-id if-chain. Structurally the same shape
/// `build_dispatch_body` emits for an actor, minus the `__set_reply_to` call
/// (the adopter's dispatch already made it) and minus the fallback tail — a
/// set that does not recognize the kind returns `DISPATCH_UNKNOWN_KIND` so the
/// adopter's own tail decides.
fn build_set_dispatch_body(handlers: &[HandlerFn], transport: SetTransport, split: bool) -> TokenStream2 {
    let receiver = if split {
        quote! { __aether_state }
    } else {
        quote! { self }
    };
    let arms = handlers.iter().map(|h| {
        let k = &h.kind_ty;
        let method = &h.method.sig.ident;
        let call = match (h.class, &h.reply) {
            (HandlerClass::Single, HandlerReply::Sync(_)) => quote! {
                let __aether_reply = Self::#method(#receiver, __aether_ctx.as_single(), __aether_decoded);
                ::aether_actor::OutboundReply::reply(__aether_ctx, &__aether_reply);
            },
            (HandlerClass::Single, _) => quote! {
                Self::#method(#receiver, __aether_ctx.as_single(), __aether_decoded);
            },
            (HandlerClass::Manual, _) => quote! {
                Self::#method(#receiver, __aether_ctx, __aether_decoded);
            },
            (HandlerClass::Multi, _) => quote! {
                Self::#method(#receiver, __aether_ctx.as_multi(), __aether_decoded);
            },
        };
        let (matches_kind, decode) = transport.arm_terms(k);
        quote! {
            if #matches_kind {
                if let ::core::option::Option::Some(__aether_decoded) = #decode {
                    #call
                    return ::aether_actor::DISPATCH_HANDLED;
                }
            }
        }
    });
    let kind_binding = transport.kind_binding();
    quote! {
        #kind_binding
        #( #arms )*
        ::aether_actor::DISPATCH_UNKNOWN_KIND
    }
}
