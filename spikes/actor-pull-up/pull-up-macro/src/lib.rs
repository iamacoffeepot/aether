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
//! A *function-like* macro invoked at the identity site:
//! ```ignore
//! #[cfg(feature = "runtime")]
//! mod runtime;                              // plain, hand-gated — no macro
//! lift_markers!(runtime => RenderCapability); // reads runtime.rs, emits markers
//! ```
//! The file module is in neither the macro's input nor its output, so #54727
//! never triggers. `Span::local_file()` (stable since 1.88) resolves the
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

    // 1. Resolve the file holding *this invocation* (the identity module).
    //    `Span::local_file()` is stable since 1.88 and gives the on-disk
    //    path, `None` only under full path remapping.
    let Some(decl_path) = module.span().unwrap().local_file() else {
        return err(
            &module,
            "lift_markers!: Span::local_file() returned None — source path \
             unavailable (path remapping?), cannot locate the runtime module file",
        );
    };

    // 2. Resolve the sibling module file: `<dir>/<name>.rs` else
    //    `<dir>/<name>/mod.rs` — the standard rule.
    let dir = decl_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let flat = dir.join(format!("{module}.rs"));
    let nested = dir.join(module.to_string()).join("mod.rs");
    let target: PathBuf = if flat.exists() { flat } else { nested };

    // 3. Read + parse (cfg-agnostic — syn keeps every item, so the kinds are
    //    harvested even in a config where `mod runtime` is stripped).
    let src = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) => return err(&module, &format!("lift_markers!: cannot read {}: {e}", target.display())),
    };
    let parsed = match syn::parse_file(&src) {
        Ok(f) => f,
        Err(e) => return err(&module, &format!("lift_markers!: parse error in {}: {e}", target.display())),
    };

    // 4. Harvest the handler kinds (type of each handler's last typed arg).
    let kinds = harvest_kinds(&parsed);
    if kinds.is_empty() {
        return err(
            &module,
            &format!("lift_markers!: no `#[handler]`-bearing impl found in {}", target.display()),
        );
    }

    // 5. Emit the always-on markers against the caller-named identity.
    let markers = kinds.iter().map(|k| quote! { impl Handles<#k> for #identity {} });
    quote! { #(#markers)* }.into()
}

/// `module => Identity`
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
