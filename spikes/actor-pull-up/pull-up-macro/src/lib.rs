//! Spike macro — the identity/runtime split with `#[actor]` on the capability.
//!
//! Target shape under test:
//! ```ignore
//! // mod.rs — identity
//! #[actor(singleton, runtime)]
//! pub struct RenderCapability;
//! #[cfg(feature = "runtime")]
//! mod runtime;
//!
//! // runtime.rs — behavior, gated with the module
//! #[runtime]
//! impl Runtime for RenderCapability {
//!     const NAMESPACE: &str = "spike.render";
//!     type State = RenderCapabilityState;
//!     fn init() -> RenderCapabilityState { .. }
//!     #[handler] fn on_tick(..) { .. }
//! }
//! ```
//!
//! - `#[actor]` sits on the **capability struct** (ordinary proc-macro input —
//!   the forbidden target was only the file module, rust#54727). It reads the
//!   sibling runtime file off disk, pulls the `NAMESPACE` const and the
//!   `#[handler]` kinds out of the impl, and emits the always-on identity
//!   surface: `impl Addressable` (namespace + cardinality resolver) and one
//!   `impl Handles<K>` per kind. Nothing is restated on the struct.
//! - `#[runtime]` sits on the impl. It emits the behavior — the `Runtime`
//!   (lifecycle + state) trait impl plus the handler bodies as inherent
//!   methods — and *consumes* the `NAMESPACE` const (routed to `Addressable`,
//!   not a member of the behavior trait). It rides the module's `#[cfg]`.
//!
//! `proc_macro::Span::local_file()` (stable since 1.88) resolves the file the
//! `#[actor]` invocation lives in; the sibling runtime file is read + parsed
//! regardless of cfg, so the lifted identity survives a build where
//! `mod runtime` is stripped.
//!
//! Stands in for `aether-actor-derive`: `Handles<K>` ≈ `HandlesKind<K>`,
//! `Addressable` ≈ the real `Addressable`, `Runtime` ≈ the gated
//! `Lifecycle`/`Dispatch`/`NativeActor` surface.

use proc_macro::TokenStream;
use quote::quote;
use std::path::PathBuf;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Expr, FnArg, ImplItem, Item, ItemImpl, ItemStruct, Token, Type, parse_macro_input,
};

/// Inert `#[handler]` shim so the consumer's `runtime.rs` compiles. The
/// harvest reads the attribute *path* off disk (never expands it).
#[proc_macro_attribute]
pub fn handler(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// `#[actor(singleton|instanced, <module>)]` on the capability struct.
///
/// Reads `<module>.rs`, pulls the `NAMESPACE` const + `#[handler]` kinds out of
/// the runtime impl, and emits the always-on identity surface against the
/// struct: `impl Addressable` and one `impl Handles<K>` per kind. Passes the
/// struct through unchanged.
#[proc_macro_attribute]
pub fn actor(attr: TokenStream, item: TokenStream) -> TokenStream {
    let ActorArgs { resolver, module } = parse_macro_input!(attr as ActorArgs);
    let item_struct = parse_macro_input!(item as ItemStruct);
    let ident = item_struct.ident.clone();

    let (namespace, kinds) = match read_identity(&module) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    let resolver_ty = match resolver {
        ResolverKind::One => quote! { One },
        ResolverKind::Many => quote! { Many },
    };
    let handles = kinds.iter().map(|k| quote! { impl Handles<#k> for #ident {} });

    quote! {
        #item_struct

        impl Addressable for #ident {
            const NAMESPACE: &'static str = #namespace;
            type Resolver = #resolver_ty;
        }

        #(#handles)*
    }
    .into()
}

/// `#[runtime]` on the behavior impl.
///
/// Splits the impl into the behavior trait impl (keeping `type State`, `fn
/// init`, and any non-handler trait items) and an inherent impl holding the
/// handler bodies, and drops the `NAMESPACE` const (it belongs to the
/// `Addressable` surface `#[actor]` emits, not the behavior trait).
#[proc_macro_attribute]
pub fn runtime(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut imp = parse_macro_input!(item as ItemImpl);
    let Some((_, trait_path, _)) = imp.trait_.clone() else {
        return err_call_site("#[runtime] expects `impl Trait for Type { .. }`");
    };
    let self_ty = imp.self_ty.clone();
    let generics = imp.generics.clone();
    let (impl_generics, _, where_clause) = generics.split_for_impl();

    let mut trait_items = Vec::new();
    let mut handler_methods = Vec::new();
    for it in std::mem::take(&mut imp.items) {
        match it {
            // Consumed: routed to `Addressable` by `#[actor]`, not part of the
            // behavior trait.
            ImplItem::Const(c) if c.ident == "NAMESPACE" => {}
            // Handler bodies move to an inherent impl (the dispatch arms call
            // them); strip the inert `#[handler]` marker on the way out.
            ImplItem::Fn(mut f) if f.attrs.iter().any(is_handler) => {
                f.attrs.retain(|a| !is_handler(a));
                handler_methods.push(ImplItem::Fn(f));
            }
            other => trait_items.push(other),
        }
    }

    quote! {
        impl #impl_generics #trait_path for #self_ty #where_clause {
            #(#trait_items)*
        }

        impl #impl_generics #self_ty #where_clause {
            #(#handler_methods)*
        }
    }
    .into()
}

/// Resolve the sibling runtime file, read + parse it, and pull the `NAMESPACE`
/// const expression and the `#[handler]` kinds out of its impl. Shared core of
/// `#[actor]`.
fn read_identity(module: &syn::Ident) -> Result<(Expr, Vec<Type>), TokenStream> {
    // `Span::local_file()` (stable since 1.88) → on-disk path of the file
    // holding the invocation; `None` only under full path remapping.
    let Some(decl_path) = module.span().unwrap().local_file() else {
        return Err(err(
            module,
            "#[actor]: Span::local_file() returned None — source path unavailable \
             (path remapping?), cannot locate the runtime module file",
        ));
    };
    let dir = decl_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let flat = dir.join(format!("{module}.rs"));
    let nested = dir.join(module.to_string()).join("mod.rs");
    let target: PathBuf = if flat.exists() { flat } else { nested };

    let src = std::fs::read_to_string(&target)
        .map_err(|e| err(module, &format!("#[actor]: cannot read {}: {e}", target.display())))?;
    let parsed = syn::parse_file(&src)
        .map_err(|e| err(module, &format!("#[actor]: parse error in {}: {e}", target.display())))?;

    for item in &parsed.items {
        let Item::Impl(imp) = item else { continue };
        let kinds = harvest_kinds(imp);
        if kinds.is_empty() {
            continue;
        }
        let Some(namespace) = namespace_const(imp) else {
            return Err(err(
                module,
                &format!(
                    "#[actor]: the runtime impl in {} has #[handler]s but no \
                     `const NAMESPACE` to lift into Addressable",
                    target.display()
                ),
            ));
        };
        return Ok((namespace, kinds));
    }
    Err(err(
        module,
        &format!("#[actor]: no `#[handler]`-bearing impl found in {}", target.display()),
    ))
}

/// Kind type of every `#[handler]` method (its last typed argument).
fn harvest_kinds(imp: &ItemImpl) -> Vec<Type> {
    imp.items
        .iter()
        .filter_map(|ii| {
            let ImplItem::Fn(f) = ii else { return None };
            if !f.attrs.iter().any(is_handler) {
                return None;
            }
            f.sig.inputs.iter().rev().find_map(|arg| match arg {
                FnArg::Typed(pt) => Some((*pt.ty).clone()),
                FnArg::Receiver(_) => None,
            })
        })
        .collect()
}

/// The `NAMESPACE` const's initializer expression, if present.
fn namespace_const(imp: &ItemImpl) -> Option<Expr> {
    imp.items.iter().find_map(|ii| match ii {
        ImplItem::Const(c) if c.ident == "NAMESPACE" => Some(c.expr.clone()),
        _ => None,
    })
}

fn is_handler(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "handler")
}

/// `singleton`/`instanced` cardinality + the runtime module name, in any order.
struct ActorArgs {
    resolver: ResolverKind,
    module: syn::Ident,
}

enum ResolverKind {
    One,
    Many,
}

impl Parse for ActorArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let idents = Punctuated::<syn::Ident, Token![,]>::parse_terminated(input)?;
        let mut resolver = ResolverKind::One;
        let mut module = None;
        for id in idents {
            match id.to_string().as_str() {
                "singleton" => resolver = ResolverKind::One,
                "instanced" => resolver = ResolverKind::Many,
                _ => module = Some(id),
            }
        }
        let module = module.ok_or_else(|| {
            input.error("#[actor] needs the runtime module name, e.g. `#[actor(singleton, runtime)]`")
        })?;
        Ok(Self { resolver, module })
    }
}

fn err(tokens: &impl quote::ToTokens, msg: &str) -> TokenStream {
    syn::Error::new_spanned(tokens, msg).to_compile_error().into()
}

fn err_call_site(msg: &str) -> TokenStream {
    syn::Error::new(proc_macro2::Span::call_site(), msg)
        .to_compile_error()
        .into()
}
