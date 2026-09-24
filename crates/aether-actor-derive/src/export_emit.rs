//! Finish an `export!` generator pipeline by emitting one keyed, no-generator
//! `export!`: `boot = B, default = D, public = [..], private = [..]`, with an
//! absent `boot` or `default` and an empty `public` or `private` omitted. The
//! pipeline's exported set still carries `boot` and `default`, so their first
//! occurrence is removed from `public`; a second one means the author listed
//! the type under `default` (or `boot`) and again under `public`, which is
//! refused here because the direct path refuses it too (a conflicting marker
//! impl).

use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, Token, Type, braced, bracketed};

pub fn emit(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(input as EmitInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

struct EmitInput {
    boot: Option<Type>,
    default: Option<Type>,
    types: Vec<Type>,
    private: Vec<Type>,
}

impl Parse for EmitInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut boot = None;
        let mut default = None;
        let mut types = Vec::new();
        let mut private = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "boot" => boot = parse_optional_type(input)?,
                "default" => default = parse_optional_type(input)?,
                "actors" => skip_braced_list(input)?,
                "exports" => types = parse_export_types(input)?,
                "private" => private = parse_export_types(input)?,
                other => return Err(syn::Error::new_spanned(&key, format!("unknown export emit field `{other}`"))),
            }
        }
        if types.is_empty() {
            return Err(syn::Error::new(Span::call_site(), "export! generators produced no types to export"));
        }
        Ok(Self { boot, default, types, private })
    }
}

fn parse_optional_type(input: ParseStream<'_>) -> syn::Result<Option<Type>> {
    if input.peek(Ident) {
        let ident: Ident = input.fork().parse()?;
        if ident == "none" {
            input.parse::<Ident>()?;
            return Ok(None);
        }
    }
    let content;
    braced!(content in input);
    Ok(Some(content.parse()?))
}

fn skip_braced_list(input: ParseStream<'_>) -> syn::Result<()> {
    let content;
    bracketed!(content in input);
    while !content.is_empty() {
        let inner;
        braced!(inner in content);
        while !inner.is_empty() {
            let _: TokenTree = inner.parse()?;
        }
    }
    Ok(())
}

fn parse_export_types(input: ParseStream<'_>) -> syn::Result<Vec<Type>> {
    let content;
    bracketed!(content in input);
    let mut types = Vec::new();
    while !content.is_empty() {
        let wrapped;
        braced!(wrapped in content);
        types.push(wrapped.parse()?);
    }
    Ok(types)
}

fn expand(input: EmitInput) -> syn::Result<TokenStream2> {
    let EmitInput { boot, default, types, private } = input;
    let mut rest: Vec<&Type> = types.iter().collect();
    for (key, slot) in [("default", default.as_ref()), ("boot", boot.as_ref())] {
        let Some(slot) = slot else {
            continue;
        };
        if let Some(first) = rest.iter().position(|ty| type_in(slot, ty)) {
            rest.remove(first);
        }
        if rest.iter().any(|ty| type_in(slot, ty)) {
            return Err(syn::Error::new_spanned(
                slot,
                format!("`{}` is listed under `{key}` and again under `public`; list it once", quote!(#slot)),
            ));
        }
    }
    if boot.is_some() && default.is_none() && rest.is_empty() {
        return Err(syn::Error::new(Span::call_site(), "export! boot-only modules need at least one non-boot export"));
    }

    let boot = boot.map(|boot| quote! { boot = #boot, });
    let default = default.map(|default| quote! { default = #default, });
    let public = (!rest.is_empty()).then(|| quote! { public = [#(#rest),*], });
    let private = (!private.is_empty()).then(|| quote! { private = [#(#private),*], });
    Ok(quote! { ::aether_actor::export! { #boot #default #public #private } })
}

fn type_in(needle: &Type, haystack: &Type) -> bool {
    quote!(#needle).to_string() == quote!(#haystack).to_string()
}
