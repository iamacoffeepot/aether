//! Re-emit the `Program` impl and the private run supertrait.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{FnArg, ImplItem, Pat, Type};

use crate::export_desc::emit_program_export_desc;
use crate::parse::ProgramDef;

pub fn expand(def: ProgramDef) -> TokenStream2 {
    let ProgramDef { mut item, self_ty, name, intent, input, result, async_run } = def;
    let run = item
        .items
        .iter()
        .find_map(|item| match item {
            ImplItem::Fn(method) if method.sig.ident == "run" => Some(method.clone()),
            _ => None,
        })
        .expect("parse requires fn run");
    item.items.retain(|item| !matches!(item, ImplItem::Fn(method) if method.sig.ident == "run"));

    let export_desc = emit_program_export_desc(&self_ty, &name, &intent, &input, &result, async_run);
    let supertrait = if async_run {
        expand_async(&self_ty, &run)
    } else {
        expand_sync(&self_ty, &run)
    };
    quote! {
        #item

        #supertrait

        #export_desc
    }
}

fn expand_sync(self_ty: &Type, run: &syn::ImplItemFn) -> TokenStream2 {
    let block = &run.block;
    let input = &run.sig.inputs[0];
    let env = &run.sig.inputs[1];
    let output = &run.sig.output;
    quote! {
        impl ::aether_bloomery_program::SyncProgram for #self_ty {
            fn run(#input, #env) #output {
                #block
            }
        }
    }
}

fn expand_async(self_ty: &Type, run: &syn::ImplItemFn) -> TokenStream2 {
    let block = &run.block;
    let input = &run.sig.inputs[0];
    let env_ident = env_ident(&run.sig.inputs[1]);
    let env_ty = owned_env_type(&run.sig.inputs[1]);
    quote! {
        impl ::aether_bloomery_program::AsyncProgram for #self_ty {
            fn run(
                #input,
                mut #env_ident: #env_ty,
            ) -> impl ::core::future::Future<
                Output = ::core::result::Result<Self::Result, ::aether_bloomery_program::Refusal>,
            > + Send + 'static {
                async move #block
            }
        }
    }
}

fn owned_env_type(arg: &FnArg) -> TokenStream2 {
    match arg {
        FnArg::Typed(typed) => match peel(&typed.ty) {
            Type::Reference(reference) => {
                let inner = &reference.elem;
                quote! { #inner }
            }
            other => quote! { #other },
        },
        FnArg::Receiver(_) => quote! { ::aether_bloomery_program::Env<::aether_bloomery_program::Async> },
    }
}

fn peel(ty: &Type) -> &Type {
    match ty {
        Type::Group(group) => peel(&group.elem),
        Type::Paren(paren) => peel(&paren.elem),
        other => other,
    }
}

fn env_ident(arg: &FnArg) -> syn::Ident {
    match arg {
        FnArg::Typed(typed) => match &*typed.pat {
            Pat::Ident(ident) => ident.ident.clone(),
            _ => syn::Ident::new("env", proc_macro2::Span::call_site()),
        },
        FnArg::Receiver(_) => syn::Ident::new("env", proc_macro2::Span::call_site()),
    }
}
