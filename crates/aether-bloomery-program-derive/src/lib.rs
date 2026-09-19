//! Proc macros for `#[program]` authoring.
//!
//! `#[program]` sits on `impl Program for Name`, checks the author form, and
//! emits the impl unchanged plus an export-descriptor companion so
//! `aether_bloomery_bundle::bundle` can select programs without reflecting on trait impls.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use syn::{ItemImpl, parse_macro_input};

mod check;
mod expand;
mod export_desc;
mod parse;

/// Outer attribute on `impl Program for Name`.
///
/// Takes no arguments. The impl must declare `NAME`, `INTENT`, `MODE = Mode::Pure`,
/// `Input`, and `Result`. `run` is an associated function: no receiver, not `async`.
#[proc_macro_attribute]
pub fn program(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "#[program] takes no arguments")
            .to_compile_error()
            .into();
    }
    let item = parse_macro_input!(item as ItemImpl);
    match parse::parse_program(item) {
        Ok(def) => expand::expand(def).into(),
        Err(error) => error.to_compile_error().into(),
    }
}
