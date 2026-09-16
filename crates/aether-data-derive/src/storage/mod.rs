//! `#[derive(Storage)]` — TLV codec, nominal `Kind::ID`, no positional mail body.

mod attr;
mod emit;
mod guard;

use proc_macro2::TokenStream as TokenStream2;
use syn::DeriveInput;

use crate::parse_optional_kind_attr;

pub fn expand_storage(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let kind = parse_optional_kind_attr(&input.attrs)?;
    let storage = attr::parse_type_storage(&input.attrs)?;
    if storage.validate && kind.is_some() {
        return Err(syn::Error::new(
            attr::flag_span(&input.attrs, "validate").unwrap_or_else(|| input.ident.span()),
            "`validate` cannot be combined with `#[kind]`",
        ));
    }
    if kind.is_none() && storage.strict {
        return Err(syn::Error::new(
            attr::flag_span(&input.attrs, "strict").unwrap_or_else(|| input.ident.span()),
            "`strict` applies to a root; this type has no kind name",
        ));
    }
    if storage.validate {
        guard::check(input, None)?;
        guard::check_validate(input)?;
        return Ok(emit::emit_validate(input));
    }
    guard::check(input, kind.as_ref())?;
    let schema_core = crate::expand_schema_core(input)?;
    let element = emit::emit_tagged_element(&input.ident);
    if let Some(kind) = kind {
        let storage_impls = emit::emit(input, &kind, &storage)?;
        Ok(quote::quote! { #schema_core #element #storage_impls })
    } else {
        let nested = emit::emit_nested(input)?;
        Ok(quote::quote! { #schema_core #element #nested })
    }
}
