//! Signature restrictions owned by `#[reactor]`.

use syn::{FnArg, Pat, ReturnType, Signature, Type};

pub fn reject_async(sig: &Signature) -> syn::Result<()> {
    if let Some(asyncness) = &sig.asyncness {
        return Err(syn::Error::new_spanned(asyncness, "#[rule] methods are synchronous; remove `async`"));
    }
    Ok(())
}

pub fn reject_const(sig: &Signature) -> syn::Result<()> {
    if let Some(constness) = &sig.constness {
        return Err(syn::Error::new_spanned(constness, "#[rule] methods cannot be `const`"));
    }
    Ok(())
}

pub fn reject_unsafe(sig: &Signature) -> syn::Result<()> {
    if let Some(unsafety) = &sig.unsafety {
        return Err(syn::Error::new_spanned(unsafety, "#[rule] methods cannot be `unsafe`"));
    }
    Ok(())
}

pub fn reject_generics(sig: &Signature) -> syn::Result<()> {
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&sig.generics, "#[rule] methods cannot be generic"));
    }
    Ok(())
}

pub fn require_immutable_self(sig: &Signature) -> syn::Result<()> {
    let Some(receiver) = sig.receiver() else {
        return Err(syn::Error::new_spanned(sig, "#[rule] methods take `&self`"));
    };
    if receiver.mutability.is_some() {
        return Err(syn::Error::new_spanned(receiver, "#[rule] methods take `&self`; mutable self is not supported"));
    }
    if receiver.reference.is_none() {
        return Err(syn::Error::new_spanned(receiver, "#[rule] methods take `&self`; owned self is not supported"));
    }
    if receiver.colon_token.is_some() {
        return Err(syn::Error::new_spanned(
            receiver,
            "#[rule] methods take `&self`; explicit self types are not supported",
        ));
    }
    Ok(())
}

pub fn reject_borrowed_param(ty: &Type) -> syn::Result<()> {
    if let Type::Reference(_) = ty {
        return Err(syn::Error::new_spanned(
            ty,
            "#[rule] parameters are owned trigger, view, and guard values; \
             engine context and borrowed parameters are unsupported",
        ));
    }
    Ok(())
}

pub fn require_output_type(sig: &Signature) -> syn::Result<Type> {
    let ReturnType::Type(_, ty) = &sig.output else {
        return Err(syn::Error::new_spanned(
            sig,
            "#[rule] methods return exactly one mail-capable typed output; unit is unsupported",
        ));
    };
    reject_wrapper(ty)?;
    Ok((**ty).clone())
}

fn reject_wrapper(ty: &Type) -> syn::Result<()> {
    match ty {
        Type::Tuple(tuple) if tuple.elems.is_empty() => Err(syn::Error::new_spanned(
            ty,
            "#[rule] methods return exactly one mail-capable typed output; unit is unsupported",
        )),
        Type::Tuple(tuple) => Err(syn::Error::new_spanned(
            tuple,
            "#[rule] methods return exactly one mail-capable typed output; tuples are unsupported",
        )),
        Type::ImplTrait(impl_trait) => Err(syn::Error::new_spanned(
            impl_trait,
            "#[rule] methods return exactly one mail-capable typed output; impl Trait is unsupported",
        )),
        Type::Never(never) => Err(syn::Error::new_spanned(
            never,
            "#[rule] methods return exactly one mail-capable typed output; the never type is unsupported",
        )),
        Type::Path(path) => {
            let Some(last) = path.path.segments.last() else {
                return Ok(());
            };
            let name = last.ident.to_string();
            if matches!(name.as_str(), "Option" | "Vec" | "Result") {
                return Err(syn::Error::new_spanned(
                    path,
                    format!("#[rule] methods return exactly one mail-capable typed output; `{name}` is unsupported"),
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn typed_inputs(sig: &Signature) -> syn::Result<Vec<&syn::PatType>> {
    let mut typed = Vec::new();
    for input in &sig.inputs {
        match input {
            FnArg::Receiver(_) => {}
            FnArg::Typed(pat) => typed.push(pat),
        }
    }
    if typed.is_empty() {
        return Err(syn::Error::new_spanned(sig, "#[rule] needs a typed trigger parameter after `&self`"));
    }
    Ok(typed)
}

pub fn param_ident(pat: &Pat) -> syn::Result<syn::Ident> {
    match pat {
        Pat::Ident(ident) if ident.subpat.is_none() => Ok(ident.ident.clone()),
        other => {
            Err(syn::Error::new_spanned(other, "view and guard parameters take a simple identifier (`heads: Heads`)"))
        }
    }
}
