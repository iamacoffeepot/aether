//! `#[derive(Storage)]` — TLV codec, nominal `Kind::ID`, no positional mail body.

mod attr;
mod emit;
mod guard;

use proc_macro2::TokenStream as TokenStream2;
use syn::DeriveInput;

use crate::parse_kind_attr;

pub fn expand_storage(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let kind = parse_kind_attr(&input.attrs)?;
    guard::check(input, &kind)?;
    let storage = attr::parse_type_storage(&input.attrs)?;
    let schema_core = crate::expand_schema_core(input)?;
    let element = emit::emit_tagged_element(&input.ident);
    let storage_impls = emit::emit(input, &kind, &storage)?;
    Ok(quote::quote! { #schema_core #element #storage_impls })
}
