//! Proc-macro home for `#[behavior]` (ADR-0137). Ports the `#[actor]`
//! template one tier over: third-parameter kind inference (extended to read
//! the `&mut K` vs `&K` intercept-vs-observe intent), an inert-marker scan
//! for `#[on]` / `#[on_attach]` / `#[on_frame]` / `#[on_detach]`, a kind-id
//! if-chain dispatch table, an ids-only exports-manifest custom section, and
//! the four guest exports (`alloc` / `filter` / `state_save` / `state_load`)
//! emitted only on `wasm`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Attribute, FnArg, ImplItem, ItemImpl, Type, parse_macro_input};

/// The lifecycle sentinel a marker attribute routes to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Attach,
    Frame,
    Detach,
}

impl Lifecycle {
    fn from_attr(attr: &Attribute) -> Option<Self> {
        let ident = attr.path().segments.last()?.ident.to_string();
        match ident.as_str() {
            "on_attach" => Some(Self::Attach),
            "on_frame" => Some(Self::Frame),
            "on_detach" => Some(Self::Detach),
            _ => None,
        }
    }

    /// The `Behavior` trait method this hook overrides.
    fn trait_method(self) -> syn::Ident {
        match self {
            Self::Attach => format_ident!("on_attach"),
            Self::Frame => format_ident!("on_frame"),
            Self::Detach => format_ident!("on_detach"),
        }
    }
}

/// A `#[on(K)]` handler and the intercept-vs-observe intent read off its
/// third parameter (`&mut K` intercepts, `&K` observes).
struct Handler {
    method: syn::Ident,
    kind_ty: Type,
    intercepts: bool,
}

/// A lifecycle hook the author marked (`#[on_attach]` etc.) on a method of
/// any name — the macro wraps it into the corresponding `Behavior` override.
struct LifecycleHook {
    hook: Lifecycle,
    method: syn::Ident,
}

/// Outer attribute on an `impl Behavior for X` block (ADR-0137). Reads the
/// `#[on]` handlers and lifecycle markers inside, then emits the inherent
/// dispatch table + exports manifest, the `impl Behavior` overrides, and the
/// four guest exports.
#[proc_macro_attribute]
pub fn behavior(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "#[behavior] takes no arguments")
            .to_compile_error()
            .into();
    }
    let item = parse_macro_input!(item as ItemImpl);
    match expand_behavior(item) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Inert marker for a `#[behavior]` mail handler. Real logic runs inside
/// `#[behavior]`, which strips this before it expands standalone; the shim
/// only exists so `#[on]` parses syntactically and rust-analyzer accepts it.
#[proc_macro_attribute]
pub fn on(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    marker_error("on")
}

/// Inert marker for the attach lifecycle hook (see [`macro@on`]).
#[proc_macro_attribute]
pub fn on_attach(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    marker_error("on_attach")
}

/// Inert marker for the per-frame lifecycle hook (see [`macro@on`]).
#[proc_macro_attribute]
pub fn on_frame(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    marker_error("on_frame")
}

/// Inert marker for the detach lifecycle hook (see [`macro@on`]).
#[proc_macro_attribute]
pub fn on_detach(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    marker_error("on_detach")
}

fn marker_error(name: &str) -> TokenStream {
    syn::Error::new(
        proc_macro2::Span::call_site(),
        format!("#[{name}] may only appear inside a `#[behavior]` impl block"),
    )
    .to_compile_error()
    .into()
}

fn attr_is_on(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|s| s.ident == "on")
}

#[allow(clippy::too_many_lines)]
fn expand_behavior(item: ItemImpl) -> syn::Result<TokenStream2> {
    let self_ty = &item.self_ty;
    let trait_ok =
        item.trait_.as_ref().and_then(|(_, path, _)| path.segments.last()).is_some_and(|s| s.ident == "Behavior");
    if !trait_ok {
        return Err(syn::Error::new_spanned(self_ty, "#[behavior] expects `impl Behavior for X`"));
    }

    let mut handlers: Vec<Handler> = Vec::new();
    let mut lifecycle_hooks: Vec<LifecycleHook> = Vec::new();
    // Author-provided items that stay verbatim: `#[on]` handler methods,
    // marked lifecycle methods, helpers (all inherent), plus any trait
    // overrides named on_attach/on_frame/on_detach/state_save/state_load.
    let mut inherent_items: Vec<ImplItem> = Vec::new();
    let mut trait_overrides: Vec<ImplItem> = Vec::new();
    let mut has_state_save = false;
    let mut has_state_load = false;

    for impl_item in item.items {
        let ImplItem::Fn(mut method) = impl_item else {
            return Err(syn::Error::new_spanned(impl_item, "#[behavior] impl blocks hold only fns"));
        };
        let name = method.sig.ident.to_string();
        let on_idx = method.attrs.iter().position(attr_is_on);
        let lifecycle_idx = method.attrs.iter().position(|a| Lifecycle::from_attr(a).is_some());

        if let Some(idx) = on_idx {
            let (kind_ty, intercepts) = extract_handler_kind(&method.sig)?;
            method.attrs.remove(idx);
            handlers.push(Handler { method: method.sig.ident.clone(), kind_ty, intercepts });
            inherent_items.push(ImplItem::Fn(method));
        } else if let Some(idx) = lifecycle_idx {
            reject_async(&method.sig, "lifecycle hooks run synchronously")?;
            let hook = Lifecycle::from_attr(&method.attrs[idx]).expect("checked present");
            method.attrs.remove(idx);
            lifecycle_hooks.push(LifecycleHook { hook, method: method.sig.ident.clone() });
            inherent_items.push(ImplItem::Fn(method));
        } else if matches!(name.as_str(), "on_attach" | "on_frame" | "on_detach") {
            // A directly-named lifecycle override stays in `impl Behavior`.
            reject_async(&method.sig, "lifecycle hooks run synchronously")?;
            trait_overrides.push(ImplItem::Fn(method));
        } else if name == "state_save" {
            has_state_save = true;
            trait_overrides.push(ImplItem::Fn(method));
        } else if name == "state_load" {
            has_state_load = true;
            trait_overrides.push(ImplItem::Fn(method));
        } else {
            inherent_items.push(ImplItem::Fn(method));
        }
    }

    reject_conflicts(&handlers, &lifecycle_hooks)?;

    let dispatch = build_dispatch_body(&handlers);
    let manifest = build_exports_manifest(&handlers);
    let dup_id_guard = build_dup_id_guard(&handlers);
    let lifecycle_overrides = lifecycle_hooks.iter().map(|h| {
        let method = &h.method;
        let trait_method = h.hook.trait_method();
        quote! {
            fn #trait_method(&mut self, __aether_ctx: &mut ::aether_behavior::BehaviorCtx) {
                self.#method(__aether_ctx);
            }
        }
    });
    let state_default = build_state_defaults(has_state_save, has_state_load);
    let exports = build_guest_exports(self_ty);

    Ok(quote! {
        impl #self_ty {
            #( #inherent_items )*

            #[doc(hidden)]
            fn __aether_behavior_dispatch(
                &mut self,
                __aether_ctx: &mut ::aether_behavior::BehaviorCtx,
                __aether_kind: ::aether_behavior::__macro_internals::KindId,
                __aether_bytes: &[u8],
            ) {
                #dispatch
            }

            #manifest
        }

        impl ::aether_behavior::Behavior for #self_ty {
            #( #trait_overrides )*
            #( #lifecycle_overrides )*
            #state_default
        }

        #exports
        #dup_id_guard
    })
}

/// Third-parameter kind inference. The handler signature is
/// `(&mut self, ctx: &mut BehaviorCtx, m: &K)` (observe) or `… m: &mut K`
/// (intercept); the referenced type is the kind, the mutability is the
/// intent.
fn extract_handler_kind(sig: &syn::Signature) -> syn::Result<(Type, bool)> {
    reject_async(sig, "#[on] handlers are synchronous - the dispatch table calls them as statements")?;
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[on] handler must have signature `(&mut self, ctx: &mut BehaviorCtx, m: &K)` \
             or `(&mut self, ctx: &mut BehaviorCtx, m: &mut K)`",
        ));
    }
    if !matches!(sig.inputs[0], FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(&sig.inputs[0], "#[on] handler's first parameter must be `&mut self`"));
    }
    let FnArg::Typed(pat) = &sig.inputs[2] else {
        return Err(syn::Error::new_spanned(
            &sig.inputs[2],
            "#[on] handler's third parameter must be a typed `m: &K` or `m: &mut K`",
        ));
    };
    let Type::Reference(reference) = &*pat.ty else {
        return Err(syn::Error::new_spanned(
            &pat.ty,
            "#[on] handler's kind parameter must be a reference — `&K` observes, `&mut K` intercepts",
        ));
    };
    let intercepts = reference.mutability.is_some();
    Ok(((*reference.elem).clone(), intercepts))
}

fn reject_async(sig: &syn::Signature, target: &str) -> syn::Result<()> {
    if let Some(asyncness) = &sig.asyncness {
        return Err(syn::Error::new_spanned(asyncness, format!("#[behavior] {target} - remove `async`")));
    }
    Ok(())
}

/// The kind-id if-chain. Sentinel arms route lifecycle to the `Behavior`
/// trait methods; kind arms decode `K`, call the handler, and set the
/// verdict — `&mut K` re-encodes and forwards the mutation, `&K` leaves the
/// default forward-original.
fn build_dispatch_body(handlers: &[Handler]) -> TokenStream2 {
    let handler_arms = handlers.iter().map(|h| {
        let method = &h.method;
        let k = &h.kind_ty;
        let call = if h.intercepts {
            quote! {
                let mut __aether_decoded = __aether_decoded;
                self.#method(__aether_ctx, &mut __aether_decoded);
                __aether_ctx.__forward_mutated(
                    ::aether_behavior::__macro_internals::Kind::encode_into_bytes(&__aether_decoded),
                );
            }
        } else {
            quote! {
                self.#method(__aether_ctx, &__aether_decoded);
            }
        };
        quote! {
            if __aether_kind == <#k as ::aether_behavior::__macro_internals::Kind>::ID {
                if let ::core::option::Option::Some(__aether_decoded) =
                    <#k as ::aether_behavior::__macro_internals::Kind>::decode_from_bytes(__aether_bytes)
                {
                    #call
                } else {
                    __aether_ctx.__fault();
                }
                return;
            }
        }
    });

    quote! {
        if __aether_kind == ::aether_behavior::sentinel::ATTACH {
            <Self as ::aether_behavior::Behavior>::on_attach(self, __aether_ctx);
            return;
        }
        if __aether_kind == ::aether_behavior::sentinel::FRAME {
            <Self as ::aether_behavior::Behavior>::on_frame(self, __aether_ctx);
            return;
        }
        if __aether_kind == ::aether_behavior::sentinel::DETACH {
            <Self as ::aether_behavior::Behavior>::on_detach(self, __aether_ctx);
            return;
        }
        #( #handler_arms )*
        let _ = (__aether_ctx, __aether_bytes);
    }
}

/// The ids-only exports manifest: a version byte then each handled kind id
/// as a little-endian `u64`. Emitted as inherent consts the guest exports
/// pin into the `aether.behavior.exports` custom section.
fn build_exports_manifest(handlers: &[Handler]) -> TokenStream2 {
    let word_count = handlers.len();
    let copy_blocks = handlers.iter().map(|h| {
        let k = &h.kind_ty;
        quote! {
            {
                let __aether_id =
                    <#k as ::aether_behavior::__macro_internals::Kind>::ID.0.to_le_bytes();
                let mut __aether_i = 0;
                while __aether_i < 8 {
                    out[pos] = __aether_id[__aether_i];
                    pos += 1;
                    __aether_i += 1;
                }
            }
        }
    });

    quote! {
        #[doc(hidden)]
        pub const __AETHER_BEHAVIOR_EXPORTS_LEN: usize = 1 + #word_count * 8;

        #[doc(hidden)]
        pub const __AETHER_BEHAVIOR_EXPORTS: [u8; Self::__AETHER_BEHAVIOR_EXPORTS_LEN] = {
            let mut out = [0u8; Self::__AETHER_BEHAVIOR_EXPORTS_LEN];
            out[0] = ::aether_behavior::__macro_internals::EXPORTS_MANIFEST_VERSION;
            let mut pos = 1;
            #( #copy_blocks )*
            let _ = pos;
            out
        };
    }
}

/// Const-eval guard for same-id handlers that token-level conflict checks
/// cannot see, such as aliases or alternate paths to the same `Kind`.
fn build_dup_id_guard(handlers: &[Handler]) -> TokenStream2 {
    let handler_count = handlers.len();
    let ids = handlers.iter().map(|h| {
        let k = &h.kind_ty;
        quote! {
            <#k as ::aether_behavior::__macro_internals::Kind>::ID.0
        }
    });

    quote! {
        const _: () = {
            const __AETHER_HANDLER_IDS: [u64; #handler_count] = [ #( #ids ),* ];
            let mut i = 0;
            while i < #handler_count {
                let mut j = i + 1;
                while j < #handler_count {
                    if __AETHER_HANDLER_IDS[i] == __AETHER_HANDLER_IDS[j] {
                        panic!("two #[on] handlers resolve to the same kind id");
                    }
                    j += 1;
                }
                i += 1;
            }
        };
    }
}

/// The default `state_save` / `state_load` bodies (serde over `Self`),
/// emitted only when the author did not override them. The `Serialize` /
/// `DeserializeOwned` bound lands at the emitted call site, so a behavior
/// leaning on the default must derive them.
fn build_state_defaults(has_state_save: bool, has_state_load: bool) -> TokenStream2 {
    let save = if has_state_save {
        quote! {}
    } else {
        quote! {
            fn state_save(&self) -> ::aether_behavior::__macro_internals::Vec<u8> {
                ::aether_behavior::__macro_internals::state_save_serde(self)
            }
        }
    };
    let load = if has_state_load {
        quote! {}
    } else {
        quote! {
            fn state_load(&mut self, __aether_bytes: &[u8]) {
                ::aether_behavior::__macro_internals::state_load_serde(self, __aether_bytes);
            }
        }
    };
    quote! { #save #load }
}

/// The four guest exports (`alloc` / `filter` / `state_save` / `state_load`),
/// emitted behind `#[cfg(target_family = "wasm")]` so they are inert on
/// native (the `aether-actor` `export!` pattern). The instance is held in a
/// module-level `Slot`, lazily constructed via `Default` on first access.
fn build_guest_exports(self_ty: &Type) -> TokenStream2 {
    quote! {
        #[cfg(target_family = "wasm")]
        const _: () = {
            static __AETHER_BEHAVIOR_SLOT:
                ::aether_behavior::__macro_internals::Slot<#self_ty> =
                ::aether_behavior::__macro_internals::Slot::new();
            static __AETHER_BEHAVIOR_MIRRORS:
                ::aether_behavior::__macro_internals::Slot<
                    ::aether_behavior::__macro_internals::MirrorStore
                > =
                ::aether_behavior::__macro_internals::Slot::new();

            #[unsafe(link_section = "aether.behavior.exports")]
            static __AETHER_BEHAVIOR_EXPORTS_SECTION:
                [u8; <#self_ty>::__AETHER_BEHAVIOR_EXPORTS_LEN] =
                <#self_ty>::__AETHER_BEHAVIOR_EXPORTS;

            /// # Safety
            /// Called by the host per the `cabi_realloc` layout contract.
            #[unsafe(export_name = "alloc")]
            pub unsafe extern "C" fn alloc(
                old_ptr: u32,
                old_size: u32,
                align: u32,
                new_size: u32,
            ) -> u32 {
                // SAFETY: the host upholds the realloc layout contract.
                unsafe {
                    ::aether_behavior::__macro_internals::realloc_bytes(
                        old_ptr as usize as *mut u8,
                        old_size as usize,
                        align as usize,
                        new_size as usize,
                    ) as usize as u32
                }
            }

            /// # Safety
            /// The host wrote `len` bytes at `ptr` before this call.
            #[unsafe(export_name = "filter")]
            pub unsafe extern "C" fn filter(kind: u64, ptr: u32, len: u32) -> u64 {
                let __aether_kind = ::aether_behavior::__macro_internals::KindId(kind);
                // SAFETY: host wrote `len` bytes at `ptr`; slice bounded here.
                let __aether_bytes =
                    unsafe { ::aether_behavior::__macro_internals::read_guest_slice(ptr, len) };
                let __aether_instance = __AETHER_BEHAVIOR_SLOT.get_or_default();
                let __aether_mirrors = __AETHER_BEHAVIOR_MIRRORS.get_or_default();
                let __aether_encoded = ::aether_behavior::__macro_internals::run_filter(
                    __aether_mirrors,
                    __aether_kind,
                    __aether_bytes,
                    |__aether_ctx| {
                        __aether_instance
                            .__aether_behavior_dispatch(__aether_ctx, __aether_kind, __aether_bytes);
                    },
                );
                ::aether_behavior::__macro_internals::leak_packed(__aether_encoded)
            }

            #[unsafe(export_name = "state_save")]
            pub extern "C" fn state_save() -> u64 {
                let __aether_instance = __AETHER_BEHAVIOR_SLOT.get_or_default();
                let __aether_bytes =
                    ::aether_behavior::Behavior::state_save(__aether_instance);
                ::aether_behavior::__macro_internals::leak_packed(__aether_bytes)
            }

            /// # Safety
            /// The host wrote `len` bytes at `ptr` before this call.
            #[unsafe(export_name = "state_load")]
            pub unsafe extern "C" fn state_load(ptr: u32, len: u32) -> u32 {
                // SAFETY: host wrote `len` bytes at `ptr`; slice bounded here.
                let __aether_bytes =
                    unsafe { ::aether_behavior::__macro_internals::read_guest_slice(ptr, len) };
                let __aether_instance = __AETHER_BEHAVIOR_SLOT.get_or_default();
                ::aether_behavior::Behavior::state_load(__aether_instance, __aether_bytes);
                0
            }
        };
    }
}

fn reject_conflicts(handlers: &[Handler], lifecycle: &[LifecycleHook]) -> syn::Result<()> {
    // Two handlers on the same kind would emit two dead if-chain arms (the
    // first shadows the second); token-equality dedup, since the macro has
    // no type resolution.
    for (i, a) in handlers.iter().enumerate() {
        for b in &handlers[i + 1..] {
            if tokens_eq(&a.kind_ty, &b.kind_ty) {
                return Err(syn::Error::new(b.method.span(), "two #[on] handlers cover the same kind"));
            }
        }
    }
    // At most one hook per lifecycle sentinel.
    for (i, a) in lifecycle.iter().enumerate() {
        for b in &lifecycle[i + 1..] {
            if a.hook == b.hook {
                return Err(syn::Error::new(
                    b.method.span(),
                    "duplicate lifecycle hook — mark at most one method per sentinel",
                ));
            }
        }
    }
    Ok(())
}

fn tokens_eq(a: &Type, b: &Type) -> bool {
    quote!(#a).to_string() == quote!(#b).to_string()
}
