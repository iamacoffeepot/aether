//! Proc macros for signature-based reactor authoring.
//!
//! `#[reactor]` sits on `impl Reactor for Name` and consumes `#[rule]` methods.
//! It emits inherent rule methods plus a `Reactor` impl whose `evaluate` /
//! `visit_arms` hooks a later actor-bundle generator can wrap. Parameter roles
//! are inferred by Rust from `Arg<_, T, Rest>` — this crate does not classify
//! view versus guard by type name.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote_spanned;
use syn::spanned::Spanned;
use syn::{ItemImpl, parse_macro_input};

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
