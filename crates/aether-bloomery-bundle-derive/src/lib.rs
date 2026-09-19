//! Proc macro for the bloomery `bundle` export generator: one root for a bundle's programs and reactors.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;

mod classify;
mod expand;
mod input;

/// Hidden `bundle` export generator. Invoked by the facade's `bundle!` hook, not by authors.
#[doc(hidden)]
#[proc_macro]
pub fn __bundle_export_generate(input: TokenStream) -> TokenStream {
    expand::generate(input)
}
