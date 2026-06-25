//! Spike macro — lifting a runtime module's handler markers up to its
//! identity module, with the kinds harvested from the runtime file on disk.
//!
//! ## What this validates
//!
//! The design question: can a macro read a *file* module (`runtime.rs`), find
//! the dispatcher's `#[handler]` kinds, and emit the always-on `Handles<K>`
//! markers at the identity site — so the typed-send compile gate works even
//! in a build where the runtime module is `#[cfg]`-stripped?
//!
//! ## What does NOT work (recorded by the spike, see README)
//!
//! The attractive form `#[pull_up] mod runtime;` is rejected on stable:
//! **file modules in proc-macro input are unstable** (E0658, rust#54727 —
//! inline `mod m { .. }` is stable, the `mod m;` file form is not). So an
//! attribute macro can never receive a file-module declaration on stable.
//!
//! ## What works
//!
//! The forbidden target is the file module *specifically*. Host the attribute
//! on the identity struct instead — ordinary proc-macro input — or use a
//! function-like macro:
//! ```ignore
//! #[lift_up(runtime)]                        // attribute on the struct: works
//! pub struct RenderCapability;
//!
//! #[cfg(feature = "runtime")]
//! mod runtime;                               // plain, hand-gated — no macro
//!
//! // or, function-like:
//! lift_markers!(runtime => RenderCapability);
//! ```
//! In both, the file module is in neither the macro's input nor its output, so
//! #54727 never triggers. `Span::local_file()` (stable since 1.88) resolves the
//! invoking file, and the sibling runtime file is read + parsed regardless of
//! cfg — so the markers land even when `mod runtime` is stripped.
//!
//! This crate stands in for `aether-actor-derive`: `Handles<K>` mirrors
//! `HandlesKind<K>`, the `#[handler]` methods mirror an `#[actor]` impl.

use proc_macro::TokenStream;
use quote::quote;
use std::path::PathBuf;
use syn::parse::{Parse, ParseStream};
use syn::{FnArg, ImplItem, Item, Type, parse_macro_input};

/// Inert `#[handler]` shim so the consumer's `runtime.rs` compiles. The
/// harvest reads the attribute *path* off disk (never expands it), so this
/// only passes the item through — same role the real `#[handler]` shim plays.
#[proc_macro_attribute]
pub fn handler(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// `#[lift_up(runtime)]` / `#[lift_up(runtime => RenderCapability)]`
///
/// The *attribute* form, attached to the identity item (the struct), not the
/// file module. The forbidden target was only the file module (`mod m;`,
/// rust#54727) — a struct is ordinary proc-macro input. The macro passes the
/// attached item through unchanged and emits the lifted `Handles<K>` markers
/// beside it, reading the kinds from the sibling runtime file exactly as
/// `lift_markers!` does. The identity defaults to the attached struct's name;
/// the `=> Ident` form overrides it.
#[proc_macro_attribute]
pub fn lift_up(attr: TokenStream, item: TokenStream) -> TokenStream {
    let LiftUpArgs { module, identity } = parse_macro_input!(attr as LiftUpArgs);
    let item: Item = parse_macro_input!(item as Item);

    // Identity: explicit `=> Ident`, else the attached struct's name.
    let identity: Type = match identity {
        Some(ty) => ty,
        None => match struct_ident(&item) {
            Some(id) => syn::parse_quote!(#id),
            None => {
                return err(
                    &module,
                    "#[lift_up] without `=> Identity` must sit on a struct (so the \
                     identity name is the attached type); add `=> Identity` otherwise",
                );
            }
        },
    };

    let kinds = match harvest_from_sibling(&module) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let markers = kinds.iter().map(|k| quote! { impl Handles<#k> for #identity {} });
    quote! {
        #item
        #(#markers)*
    }
    .into()
}

/// `lift_markers!(runtime => RenderCapability)`
///
/// Reads the sibling `runtime.rs` (or `runtime/mod.rs`), harvests the
/// `#[handler]` kinds from its dispatcher impl, and emits
/// `impl Handles<K> for RenderCapability {}` per kind — always-on, at the
/// invocation site. Emits nothing else: the `mod runtime;` declaration stays
/// a plain (hand-gated) line the author writes.
#[proc_macro]
pub fn lift_markers(input: TokenStream) -> TokenStream {
    let LiftArgs { module, identity } = parse_macro_input!(input as LiftArgs);
    let kinds = match harvest_from_sibling(&module) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let markers = kinds.iter().map(|k| quote! { impl Handles<#k> for #identity {} });
    quote! { #(#markers)* }.into()
}

/// Resolve the sibling runtime file next to the invocation, read + parse it
/// (cfg-agnostically), and harvest the `#[handler]` kinds. Shared by both the
/// function-like (`lift_markers!`) and attribute (`#[lift_up]`) forms — the
/// only difference between them is where the markers are emitted.
fn harvest_from_sibling(module: &syn::Ident) -> Result<Vec<Type>, TokenStream> {
    // `Span::local_file()` (stable since 1.88) gives the on-disk path of the
    // file holding the invocation; `None` only under full path remapping.
    let Some(decl_path) = module.span().unwrap().local_file() else {
        return Err(err(
            module,
            "lift: Span::local_file() returned None — source path unavailable \
             (path remapping?), cannot locate the runtime module file",
        ));
    };
    // Standard module-file rule: `<dir>/<name>.rs` else `<dir>/<name>/mod.rs`.
    let dir = decl_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let flat = dir.join(format!("{module}.rs"));
    let nested = dir.join(module.to_string()).join("mod.rs");
    let target: PathBuf = if flat.exists() { flat } else { nested };

    let src = std::fs::read_to_string(&target)
        .map_err(|e| err(module, &format!("lift: cannot read {}: {e}", target.display())))?;
    let parsed = syn::parse_file(&src)
        .map_err(|e| err(module, &format!("lift: parse error in {}: {e}", target.display())))?;

    let kinds = harvest_kinds(&parsed);
    if kinds.is_empty() {
        return Err(err(
            module,
            &format!("lift: no `#[handler]`-bearing impl found in {}", target.display()),
        ));
    }
    Ok(kinds)
}

/// The attached struct's name, if the item is a struct (the default identity
/// for `#[lift_up]`).
fn struct_ident(item: &Item) -> Option<syn::Ident> {
    match item {
        Item::Struct(s) => Some(s.ident.clone()),
        _ => None,
    }
}

/// `module => Identity` (function-like form — identity is mandatory).
struct LiftArgs {
    module: syn::Ident,
    identity: Type,
}

impl Parse for LiftArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let module: syn::Ident = input.parse()?;
        input.parse::<syn::Token![=>]>()?;
        let identity: Type = input.parse()?;
        Ok(Self { module, identity })
    }
}

/// `module` or `module => Identity` (attribute form — identity optional,
/// defaults to the attached struct).
struct LiftUpArgs {
    module: syn::Ident,
    identity: Option<Type>,
}

impl Parse for LiftUpArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let module: syn::Ident = input.parse()?;
        let identity = if input.peek(syn::Token![=>]) {
            input.parse::<syn::Token![=>]>()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(Self { module, identity })
    }
}

/// Collect the kind type of every `#[handler]`-tagged method across all impl
/// blocks in the file — the type of the method's last typed (non-receiver) arg.
fn harvest_kinds(file: &syn::File) -> Vec<Type> {
    let mut kinds = Vec::new();
    for item in &file.items {
        let Item::Impl(imp) = item else { continue };
        for ii in &imp.items {
            let ImplItem::Fn(f) = ii else { continue };
            if !f.attrs.iter().any(is_handler) {
                continue;
            }
            if let Some(ty) = f.sig.inputs.iter().rev().find_map(|arg| match arg {
                FnArg::Typed(pt) => Some((*pt.ty).clone()),
                FnArg::Receiver(_) => None,
            }) {
                kinds.push(ty);
            }
        }
    }
    kinds
}

/// Match `#[handler]` / `#[..::handler]` by the path's last segment.
fn is_handler(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "handler")
}

fn err(tokens: &impl quote::ToTokens, msg: &str) -> TokenStream {
    syn::Error::new_spanned(tokens, msg).to_compile_error().into()
}
