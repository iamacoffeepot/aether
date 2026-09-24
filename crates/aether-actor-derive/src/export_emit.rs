//! Finish an `export!` generator pipeline by emitting a no-generator `export!`,
//! carrying the pipeline's `private` list into its `private = [..]` slot.

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
    let private = (!private.is_empty()).then(|| quote! { private = [#(#private),*], });
    let rest: Vec<&Type> = types
        .iter()
        .filter(|ty| default.as_ref().is_none_or(|default| !type_in(default, ty)))
        .filter(|ty| boot.as_ref().is_none_or(|boot| !type_in(boot, ty)))
        .collect();
    Ok(match (boot.as_ref(), default.as_ref()) {
        (Some(boot), Some(default)) => {
            quote! { ::aether_actor::export!(boot = #boot, default = #default, #(#rest,)* #private); }
        }
        (None, Some(default)) => {
            quote! { ::aether_actor::export!(default = #default, #(#rest,)* #private); }
        }
        (Some(_), None) if rest.is_empty() => {
            return Err(syn::Error::new(
                Span::call_site(),
                "export! boot-only modules need at least one non-boot export",
            ));
        }
        (Some(boot), None) => {
            quote! { ::aether_actor::export!(boot = #boot, #(#rest,)* #private); }
        }
        (None, None) if rest.len() == 1 => {
            let ty = rest[0];
            private.map_or_else(
                || quote! { ::aether_actor::export!(#ty); },
                |private| quote! { ::aether_actor::export!(#ty, #private); },
            )
        }
        (None, None) => quote! { ::aether_actor::export!(#(#rest,)* #private); },
    })
}

fn type_in(needle: &Type, haystack: &Type) -> bool {
    quote!(#needle).to_string() == quote!(#haystack).to_string()
}
