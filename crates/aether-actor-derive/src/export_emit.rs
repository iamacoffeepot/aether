//! Finish an `export!` generator pipeline by emitting a no-generator `export!`.
//!
//! Types that appear in `exports` but not in `actors` are generated inline
//! factories. They stay reachable to replacement reconstruction and are not
//! independently spawnable module exports.

use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Token, Type, braced, bracketed};

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
    actor_types: Vec<Type>,
    types: Vec<Type>,
}

impl Parse for EmitInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut boot = None;
        let mut default = None;
        let mut actor_types = Vec::new();
        let mut types = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "boot" => boot = parse_optional_type(input)?,
                "default" => default = parse_optional_type(input)?,
                "actors" => actor_types = parse_actor_types(input)?,
                "exports" => types = parse_export_types(input)?,
                other => return Err(syn::Error::new_spanned(&key, format!("unknown export emit field `{other}`"))),
            }
        }
        if types.is_empty() {
            return Err(syn::Error::new(Span::call_site(), "export! generators produced no types to export"));
        }
        Ok(Self { boot, default, actor_types, types })
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

fn parse_actor_types(input: ParseStream<'_>) -> syn::Result<Vec<Type>> {
    let content;
    bracketed!(content in input);
    let mut types = Vec::new();
    while !content.is_empty() {
        let wrapped;
        braced!(wrapped in content);
        types.push(parse_envelope_ty(&wrapped)?);
    }
    Ok(types)
}

fn parse_envelope_ty(input: ParseStream<'_>) -> syn::Result<Type> {
    let mut ty = None;
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        match key.to_string().as_str() {
            "ty" => {
                let inner;
                braced!(inner in input);
                ty = Some(inner.parse()?);
            }
            "namespace" => {
                if input.peek(LitStr) {
                    let _: LitStr = input.parse()?;
                } else if input.peek(Token![_]) {
                    input.parse::<Token![_]>()?;
                } else {
                    return Err(input.error("classified namespace must be a string literal or `_`"));
                }
            }
            "extensions" => {
                let inner;
                bracketed!(inner in input);
                while !inner.is_empty() {
                    let _: TokenTree = inner.parse()?;
                }
            }
            other => return Err(syn::Error::new_spanned(&key, format!("unknown classified field `{other}`"))),
        }
    }
    ty.ok_or_else(|| syn::Error::new(Span::call_site(), "actor envelope missing ty"))
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
    let EmitInput { boot, default, actor_types, types } = input;
    let (public, reconstruct_extras) = split_public_and_reconstruct(&actor_types, &types);
    if public.is_empty() {
        return Err(syn::Error::new(Span::call_site(), "export! generators produced no public export types"));
    }
    let rest: Vec<&Type> = public
        .iter()
        .copied()
        .filter(|ty| default.as_ref().is_none_or(|default| !type_in(default, ty)))
        .filter(|ty| boot.as_ref().is_none_or(|boot| !type_in(boot, ty)))
        .collect();
    if reconstruct_extras.is_empty() {
        emit_public_export(boot.as_ref(), default.as_ref(), &rest)
    } else {
        emit_with_reconstruct(boot.as_ref(), default.as_ref(), &rest, &reconstruct_extras)
    }
}

fn split_public_and_reconstruct<'a>(actor_types: &'a [Type], types: &'a [Type]) -> (Vec<&'a Type>, Vec<&'a Type>) {
    if actor_types.is_empty() {
        return (types.iter().collect(), Vec::new());
    }
    let public = types.iter().filter(|ty| actor_types.iter().any(|actor| type_in(ty, actor))).collect();
    let extras = types.iter().filter(|ty| !actor_types.iter().any(|actor| type_in(ty, actor))).collect();
    (public, extras)
}

fn emit_public_export(boot: Option<&Type>, default: Option<&Type>, rest: &[&Type]) -> syn::Result<TokenStream2> {
    Ok(match (boot, default) {
        (Some(boot), Some(default)) => {
            quote! { ::aether_actor::export!(boot = #boot, default = #default, #(#rest,)*); }
        }
        (None, Some(default)) => {
            quote! { ::aether_actor::export!(default = #default, #(#rest,)*); }
        }
        (Some(_), None) if rest.is_empty() => {
            return Err(syn::Error::new(
                Span::call_site(),
                "export! boot-only modules need at least one non-boot export",
            ));
        }
        (Some(boot), None) => {
            quote! { ::aether_actor::export!(boot = #boot, #(#rest,)*); }
        }
        (None, None) if rest.len() == 1 => {
            let ty = rest[0];
            quote! { ::aether_actor::export!(#ty); }
        }
        (None, None) => quote! { ::aether_actor::export!(#(#rest,)*); },
    })
}

fn emit_with_reconstruct(
    boot: Option<&Type>,
    default: Option<&Type>,
    rest: &[&Type],
    reconstruct: &[&Type],
) -> syn::Result<TokenStream2> {
    Ok(match (boot, default) {
        (Some(boot), Some(default)) => {
            quote! {
                ::aether_actor::__export_multi_internal!(
                    @boot #boot ;
                    @default #default ;
                    @all #boot, #default #(, #rest)*
                    ; @reconstruct #(#reconstruct),+
                );
            }
        }
        (None, Some(default)) => {
            quote! {
                ::aether_actor::__export_multi_internal!(
                    @no_boot ;
                    @default #default ;
                    @all #default #(, #rest)*
                    ; @reconstruct #(#reconstruct),+
                );
            }
        }
        (Some(_), None) if rest.is_empty() => {
            return Err(syn::Error::new(
                Span::call_site(),
                "export! boot-only modules need at least one non-boot export",
            ));
        }
        (Some(boot), None) => {
            quote! {
                ::aether_actor::__export_multi_internal!(
                    @boot #boot ;
                    @no_default ;
                    @all #boot #(, #rest)*
                    ; @reconstruct #(#reconstruct),+
                );
            }
        }
        (None, None) if rest.len() == 1 => {
            let ty = rest[0];
            quote! { ::aether_actor::__export_internal!(#ty ; @reconstruct #(#reconstruct),+); }
        }
        (None, None) => {
            quote! {
                ::aether_actor::__export_multi_internal!(
                    @no_boot ;
                    @no_default ;
                    @all #(#rest),+
                    ; @reconstruct #(#reconstruct),+
                );
            }
        }
    })
}

fn type_in(needle: &Type, haystack: &Type) -> bool {
    quote!(#needle).to_string() == quote!(#haystack).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn compact(tokens: &TokenStream2) -> String {
        tokens.to_string().chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[test]
    fn ordinary_exports_keep_the_public_export_form() {
        let tokens = expand(EmitInput {
            boot: None,
            default: Some(parse_quote!(Probe)),
            actor_types: vec![parse_quote!(Probe), parse_quote!(Sink)],
            types: vec![parse_quote!(Probe), parse_quote!(Sink)],
        })
        .expect("ordinary emit");
        let compact = compact(&tokens);
        assert!(compact.contains("export!(default=Probe,Sink"));
        assert!(!compact.contains("@reconstruct"));
    }

    #[test]
    fn generated_peers_are_reconstruct_only() {
        let tokens = expand(EmitInput {
            boot: None,
            default: Some(parse_quote!(Probe)),
            actor_types: vec![
                parse_quote!(Probe),
                parse_quote!(Publisher),
                parse_quote!(__AetherBloomeryReactorCluster),
                parse_quote!(Sink),
            ],
            types: vec![
                parse_quote!(Probe),
                parse_quote!(__AetherBloomeryReactorCluster),
                parse_quote!(__AetherBloomeryReactorPeer_n1),
                parse_quote!(Sink),
            ],
        })
        .expect("reconstruct emit");
        let compact = compact(&tokens);
        assert!(compact.contains("@defaultProbe"));
        assert!(compact.contains("@allProbe,__AetherBloomeryReactorCluster,Sink"));
        assert!(!compact.contains("@allProbe,__AetherBloomeryReactorCluster,__AetherBloomeryReactorPeer_n1,Sink"));
        assert!(compact.contains("@reconstruct__AetherBloomeryReactorPeer_n1"));
        assert!(!compact.contains("init_typed"));
    }

    #[test]
    fn reactor_only_keeps_a_single_public_export() {
        let tokens = expand(EmitInput {
            boot: None,
            default: None,
            actor_types: vec![parse_quote!(Publisher), parse_quote!(__AetherBloomeryReactorCluster)],
            types: vec![parse_quote!(__AetherBloomeryReactorCluster), parse_quote!(__AetherBloomeryReactorPeer_n1)],
        })
        .expect("reactor-only emit");
        let compact = compact(&tokens);
        assert!(
            compact.contains(
                "__export_internal!(__AetherBloomeryReactorCluster;@reconstruct__AetherBloomeryReactorPeer_n1)"
            )
        );
        assert!(!compact.contains("__export_multi_internal!"));
    }
}
