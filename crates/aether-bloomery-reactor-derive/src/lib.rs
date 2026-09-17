//! Proc macros for signature-based reactor authoring.
//!
//! `#[reactor]` sits on `impl Reactor for Name` and consumes `#[rule]` methods.
//! It emits inherent rule methods plus a `Reactor` impl whose `evaluate` /
//! `visit_arms` hooks [`macro@reactor_bundle`] so authors do not write actor
//! wrappers. Parameter roles are inferred by Rust from `Arg<_, T, Rest>` —
//! this crate does not classify view versus guard by type name.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote_spanned;
use syn::spanned::Spanned;
use syn::{ItemImpl, parse_macro_input};

mod bundle;
mod check;
mod expand;
mod parse;
mod pattern;

/// Outer attribute on `impl Reactor for Name`. Consumes `#[rule]` methods
/// and generates preparation/evaluation plus associated-type visitors.
///
/// Takes no arguments. The impl must declare `const NAME` and at least one
/// `#[rule]`. Each rule takes `&self`, a typed trigger, then owned view or
/// guard parameters, and returns exactly one mail-capable output.
#[proc_macro_attribute]
pub fn reactor(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(Span::call_site(), "#[reactor] takes no arguments").to_compile_error().into();
    }
    let item = parse_macro_input!(item as ItemImpl);
    match parse::parse_reactor(item) {
        Ok(def) => expand::expand(def).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Generate one views owner and one inline reactor peer per listed reactor.
///
/// `default` names the views owner type that `wire` instantiates peers under.
/// That type is the cluster role to load; packing the generated types in
/// `export!` does not spawn the peers. `namespace` is the views owner's
/// `NAMESPACE`; each peer is `{namespace}.{snake_case(Reactor)}`.
///
/// ```ignore
/// reactor_bundle! {
///     default = SourcePublicationViews,
///     namespace = "test.bloomery.reactor",
///     SourcePublisher,
///     SourceWitness,
/// }
/// ```
#[proc_macro]
pub fn reactor_bundle(input: TokenStream) -> TokenStream {
    let def = parse_macro_input!(input as bundle::BundleDef);
    match bundle::expand(def) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Marker consumed by [`macro@reactor`]. Reaching this expansion means the
/// enclosing impl is missing `#[reactor]`.
#[proc_macro_attribute]
pub fn rule(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    quote_spanned! { item.span() =>
        ::core::compile_error!("#[rule] may only appear inside a #[reactor] impl block");
        #item
    }
    .into()
}
