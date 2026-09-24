//! Signature restrictions owned by `#[program]`.

use syn::{FnArg, GenericArgument, Ident, PathArguments, Signature, Type};

pub enum EnvMarker {
    Sync,
    Async,
}

pub fn pair_run_with_env(sig: &Signature) -> syn::Result<bool> {
    let async_run = sig.asyncness.is_some();
    let env_ty = env_arg_type(sig)?;
    match (async_run, env_marker(env_ty)?) {
        (true, EnvMarker::Async) => Ok(true),
        (false, EnvMarker::Sync) => Ok(false),
        (true, EnvMarker::Sync) => {
            Err(syn::Error::new_spanned(sig.asyncness, "#[program] async fn run requires Env<Async>"))
        }
        (false, EnvMarker::Async) => Err(syn::Error::new_spanned(env_ty, "#[program] fn run requires Env<Sync>")),
    }
}

pub fn reject_run_receiver(sig: &Signature) -> syn::Result<()> {
    if let Some(receiver) = sig.receiver() {
        return Err(syn::Error::new_spanned(
            receiver,
            "#[program] run must not take a receiver; Program::run is associated",
        ));
    }
    Ok(())
}

/// Names `#[program]` accepts for a trailing binding: the closed set of program
/// APIs, each a row in `aether_bloomery_program::__macro_internals::api_target`.
const API_NAMES: [&str; 2] = ["Http", "Process"];

/// One trailing binding: the parameter, the type the author wrote, and the
/// canonical API name that type ends in.
pub struct ApiBinding {
    pub ident: Ident,
    pub ty: Type,
    pub name: Ident,
}

pub fn trailing_apis(sig: &Signature, async_run: bool) -> syn::Result<Vec<ApiBinding>> {
    let extra = sig.inputs.iter().skip(2);
    if !async_run && extra.clone().next().is_some() {
        return Err(syn::Error::new_spanned(
            sig,
            "#[program] trailing cap bindings require async fn run after Env<Async>",
        ));
    }
    let mut apis = Vec::new();
    for arg in extra {
        let FnArg::Typed(typed) = arg else {
            return Err(syn::Error::new_spanned(arg, "#[program] trailing cap bindings must be typed parameters"));
        };
        let syn::Pat::Ident(ident) = &*typed.pat else {
            return Err(syn::Error::new_spanned(
                &typed.pat,
                "#[program] trailing cap bindings must be ident parameters",
            ));
        };
        apis.push(ApiBinding { ident: ident.ident.clone(), ty: (*typed.ty).clone(), name: api_name(&typed.ty)? });
    }
    Ok(apis)
}

/// The canonical API name a binding's type ends in, in whatever path the author
/// wrote it.
fn api_name(ty: &Type) -> syn::Result<Ident> {
    let not_an_api = || syn::Error::new_spanned(ty, "#[program] cap bindings are `Http` or `Process`");
    let Type::Path(path) = peel(ty) else {
        return Err(not_an_api());
    };
    if path.qself.is_some() {
        return Err(not_an_api());
    }
    let Some(last) = path.path.segments.last() else {
        return Err(not_an_api());
    };
    if !matches!(last.arguments, PathArguments::None) || !API_NAMES.iter().any(|name| last.ident == name) {
        return Err(not_an_api());
    }
    Ok(last.ident.clone())
}

fn env_arg_type(sig: &Signature) -> syn::Result<&Type> {
    let Some(FnArg::Typed(env)) = sig.inputs.iter().nth(1) else {
        return Err(syn::Error::new_spanned(
            sig,
            "#[program] run takes `(input: Self::Input, env: &mut Env<Sync | Async>, …bindings)`",
        ));
    };
    if sig.inputs.len() > 2 && env_marker(&env.ty).is_err() {
        return Err(syn::Error::new_spanned(&env.ty, "#[program] env is the second argument; cap bindings follow env"));
    }
    Ok(&env.ty)
}

fn env_marker(ty: &Type) -> syn::Result<EnvMarker> {
    let Type::Reference(reference) = peel(ty) else {
        return Err(syn::Error::new_spanned(ty, "#[program] run env must be `&mut Env<Sync>` or `&mut Env<Async>`"));
    };
    if reference.mutability.is_none() {
        return Err(syn::Error::new_spanned(ty, "#[program] run env must be `&mut Env<Sync>` or `&mut Env<Async>`"));
    }
    let Type::Path(path) = peel(&reference.elem) else {
        return Err(syn::Error::new_spanned(ty, "#[program] run env must be `Env<Sync>` or `Env<Async>`"));
    };
    let Some(last) = path.path.segments.last() else {
        return Err(syn::Error::new_spanned(ty, "#[program] run env must be `Env<Sync>` or `Env<Async>`"));
    };
    if last.ident != "Env" {
        return Err(syn::Error::new_spanned(ty, "#[program] run env must be `Env<Sync>` or `Env<Async>`"));
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return Err(syn::Error::new_spanned(ty, "#[program] run env must be `Env<Sync>` or `Env<Async>`"));
    };
    let Some(GenericArgument::Type(marker_ty)) = args.args.first() else {
        return Err(syn::Error::new_spanned(ty, "#[program] run env must be `Env<Sync>` or `Env<Async>`"));
    };
    let Type::Path(marker) = peel(marker_ty) else {
        return Err(syn::Error::new_spanned(marker_ty, "#[program] Env marker must be Sync or Async"));
    };
    let Some(ident) = marker.path.segments.last().map(|segment| &segment.ident) else {
        return Err(syn::Error::new_spanned(marker_ty, "#[program] Env marker must be Sync or Async"));
    };
    if ident == "Sync" {
        Ok(EnvMarker::Sync)
    } else if ident == "Async" {
        Ok(EnvMarker::Async)
    } else {
        Err(syn::Error::new_spanned(marker_ty, "#[program] Env marker must be Sync or Async"))
    }
}

fn peel(ty: &Type) -> &Type {
    match ty {
        Type::Group(group) => peel(&group.elem),
        Type::Paren(paren) => peel(&paren.elem),
        other => other,
    }
}
