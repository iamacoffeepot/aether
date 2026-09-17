//! Proc macros for signature-based reactor authoring.
//!
//! `#[reactor]` sits on `impl Reactor for Name` and consumes `#[rule]` methods.
//! It emits inherent rule methods plus a `Reactor` impl whose `evaluate` /
//! `visit_arms` plus a framework descriptor extension so `bundle_reactors` can
//! wrap authored reactors without a second export macro. Parameter roles are
//! inferred by Rust from `Arg<_, T, Rest>` — this crate does not classify view
//! versus guard by type name.
//! An authored reactor must be a unit struct: the expansion checks that the
//! declared type can be constructed as a unit value, with no stored fields.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote_spanned;
use syn::spanned::Spanned;
use syn::{ItemImpl, parse_macro_input};

mod bundle;
mod check;
mod expand;
mod export_desc;
mod parse;
mod pattern;

/// Outer attribute on `impl Reactor for Name`. Consumes `#[rule]` methods
/// and generates preparation/evaluation plus associated-type visitors.
///
/// Takes no arguments. The impl must declare `const NAMESPACE` and at least one
/// `#[rule]`. The reactor type must be a unit struct. Each rule takes `&self`,
/// a typed trigger, then owned view or guard parameters, and returns exactly
/// one mail-capable output.
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

/// Hidden `bundle_reactors` export generator. Invoked by
/// [`aether_bloomery_reactor::bundle_reactors`], not by authors.
#[doc(hidden)]
#[proc_macro]
pub fn __reactor_export_generate(input: TokenStream) -> TokenStream {
    bundle::generate(input)
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
