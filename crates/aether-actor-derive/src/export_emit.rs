//! Finish an `export!` generator pipeline on the multi-actor emitter (the
//! hidden `__export_parse!(@generated ..)` row), so even a one-type generated
//! set keeps its `ActorBoundary` (ADR-0241 §3). The exported set is `boot`,
//! then the rest. The pipeline's exported set still carries `boot`, so its
//! first occurrence is removed from the rest; a second one means the author
//! listed the type under `boot` and again under `public`, which is refused
//! here because the direct path refuses it too (a conflicting marker impl).

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
    types: Vec<Type>,
    private: Vec<Type>,
}

impl Parse for EmitInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut boot = None;
        let mut types = Vec::new();
        let mut private = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "boot" => boot = parse_optional_type(input)?,
                "actors" => skip_braced_list(input)?,
                "exports" => types = parse_export_types(input)?,
                "private" => private = parse_export_types(input)?,
                other => return Err(syn::Error::new_spanned(&key, format!("unknown export emit field `{other}`"))),
            }
        }
        if types.is_empty() {
            return Err(syn::Error::new(Span::call_site(), "export! generators produced no types to export"));
        }
        Ok(Self { boot, types, private })
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
    let EmitInput { boot, types, private } = input;
    let mut rest: Vec<&Type> = types.iter().collect();
    if let Some(slot) = boot.as_ref() {
        if let Some(first) = rest.iter().position(|ty| type_in(slot, ty)) {
            rest.remove(first);
        }
        if rest.iter().any(|ty| type_in(slot, ty)) {
            return Err(syn::Error::new_spanned(
                slot,
                format!("`{}` is listed under `boot` and again under `public`; list it once", quote!(#slot)),
            ));
        }
    }
    if boot.is_some() && rest.is_empty() {
        return Err(syn::Error::new(Span::call_site(), "export! boot-only modules need at least one non-boot export"));
    }

    let boot_slot = boot.as_ref().map_or_else(|| quote! { none }, |boot| quote! { { #boot } });
    let all = boot.iter().chain(rest.iter().copied());
    Ok(quote! {
        ::aether_actor::__export_parse! {
            @generated { boot: #boot_slot, all: [#({ #all })*], private: [#({ #private })*] }
        }
    })
}

fn type_in(needle: &Type, haystack: &Type) -> bool {
    quote!(#needle).to_string() == quote!(#haystack).to_string()
}
