//! Signature restrictions owned by `#[program]`.

use syn::{FnArg, GenericArgument, PathArguments, Signature, Type};

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

fn env_arg_type(sig: &Signature) -> syn::Result<&Type> {
    let Some(FnArg::Typed(env)) = sig.inputs.iter().nth(1) else {
        return Err(syn::Error::new_spanned(
            sig,
            "#[program] run takes `(input: Self::Input, env: &mut Env<Sync | Async>)`",
        ));
    };
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
