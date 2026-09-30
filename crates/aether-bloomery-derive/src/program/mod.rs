//! `#[program]`: bloomery program authoring.
//!
//! `#[program]` sits on `impl Program for Name`, checks the author form, and
//! emits the `Program` impl without `run`, a private sync or async run
//! supertrait, and an export-descriptor companion so
//! `aether_bloomery_program::bundle` can select programs without reflecting on trait impls.

use proc_macro::TokenStream;
use syn::{ItemImpl, parse_macro_input};

mod check;
mod expand;
mod export_desc;
mod parse;

/// Expand `#[program]` on an `impl Program for Name` block.
pub fn program_attribute(attr: &TokenStream, item: TokenStream) -> TokenStream {
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
