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
//! - `__AETHER_HANDLER_SET_MANIFEST` — the set's `aether.kinds.inputs` record
//!   bytes, in the same encoding `build_inputs_manifest_consts` emits, so an
//!   adopter's manifest reports its full receive surface (ADR-0033). Typed
//!   `&'static [u8]` rather than `[u8; N]`: an associated const whose *type*
//!   mentions `Self::LEN` is not expressible on a trait, and `<[u8]>::len` is
//!   const, so the adopter recovers the length for its own array arithmetic.
//!
//! The set's `HandlesKind` markers are **not** emitted. The orphan rule
//! forecloses the set declaring them itself — `impl<T: Set> HandlesKind<K> for T`
//! puts the `Self` type parameter ahead of the first local type in the trait
//! reference — so they would have to travel through a generated
//! `macro_rules!`, and a macro-expanded `#[macro_export]` macro cannot be named
//! by absolute path from inside its own crate, which is the case every in-tree
//! adopter is in. The consequence is scoped: a set's kinds are not sendable
//! through the typed resolver (`ctx.actor::<R>().send(&k)`), while the by-name
//! parent-to-child path (`RelativeMailbox::send<K: Kind>`) carries no such
//! bound and is unaffected.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
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

    /// The inbound mail type the dispatch method matches on.
    fn mail_ty(self) -> TokenStream2 {
        match self {
            Self::Wasm => quote! { ::aether_actor::Mail<'_> },
            Self::Native => quote! { &::aether_substrate::actor::native::envelope::Envelope<'_> },
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
            SetTransport::Native => extract_native_actor_handler_kind(&f.sig, this_split)?.0,
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
    let mail_ty = transport.mail_ty();
    let dispatch_body = build_set_dispatch_body(&handlers, transport, split);
    let manifest_const = build_handler_set_manifest_const(&handlers);

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
            __aether_mail: #mail_ty,
        ) -> u32 {
            #dispatch_body
        }
    });
    item.items.push(syn::parse_quote! {
        #[doc(hidden)]
        const __AETHER_HANDLER_SET_MANIFEST: &'static [u8] = #manifest_const;
    });
    if !state_assoc.is_empty() {
        item.items.push(syn::parse_quote! { #state_assoc });
    }

    Ok(quote! { #item })
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
        let decode = match transport {
            SetTransport::Wasm => quote! { __aether_mail.decode_kind::<#k>() },
            SetTransport::Native => quote! {
                <#k as ::aether_actor::__macro_internals::Kind>::decode_from_bytes(__aether_mail.payload())
            },
        };
        quote! {
            if __aether_kind == <#k as ::aether_actor::__macro_internals::Kind>::ID {
                if let ::core::option::Option::Some(__aether_decoded) = #decode {
                    #call
                    return ::aether_actor::DISPATCH_HANDLED;
                }
            }
        }
    });
    quote! {
        // Both transports expose the inbound kind id under the same name.
        let __aether_kind = __aether_mail.kind();
        #( #arms )*
        ::aether_actor::DISPATCH_UNKNOWN_KIND
    }
}
