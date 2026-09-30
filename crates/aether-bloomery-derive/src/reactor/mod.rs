//! `#[reactor]` / `#[rule]`: signature-based reactor authoring.
//!
//! `#[reactor]` sits on `impl Reactor for Name` and consumes `#[rule]` methods.
//! It emits inherent rule methods plus a `Reactor` impl whose `evaluate` /
//! `visit_arms` plus a framework descriptor extension so `aether_bloomery_program::bundle` can
//! wrap authored reactors without a second export macro. Parameter roles are
//! inferred by Rust from `Arg<_, T, Rest>` — this module does not classify a
//! view, guard, `cited: Cited`, or `at: At` parameter by type name.
//! An authored reactor must be a unit struct: the expansion checks that the
//! declared type can be constructed as a unit value, with no stored fields.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote_spanned;
use syn::spanned::Spanned;
use syn::{ItemImpl, parse_macro_input};

mod check;
mod expand;
mod export_desc;
mod parse;
mod pattern;

/// Expand `#[reactor]` on an `impl Reactor for Name` block.
pub fn reactor_attribute(attr: &TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(Span::call_site(), "#[reactor] takes no arguments").to_compile_error().into();
    }
    let item = parse_macro_input!(item as ItemImpl);
    match parse::parse_reactor(item) {
        Ok(def) => expand::expand(def).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Expand a `#[rule]` that no `#[reactor]` consumed.
pub fn rule_attribute(item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    quote_spanned! { item.span() =>
        ::core::compile_error!("#[rule] may only appear inside a #[reactor] impl block");
        #item
    }
    .into()
}
