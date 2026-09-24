//! Re-emit the `Program` impl and the private run supertrait.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{FnArg, ImplItem, Pat, ReturnType, Type};

use crate::check::ApiBinding;
use crate::export_desc::emit_program_export_desc;
use crate::parse::ProgramDef;

pub fn expand(def: ProgramDef) -> TokenStream2 {
    let export_desc = emit_program_export_desc(&def);
    let ProgramDef { mut item, self_ty, name: _, intent: _, async_run, sampled, apis } = def;
    let run = item
        .items
        .iter()
        .find_map(|item| match item {
            ImplItem::Fn(method) if method.sig.ident == "run" => Some(method.clone()),
            _ => None,
        })
        .expect("parse requires fn run");
    item.items.retain(|item| !matches!(item, ImplItem::Fn(method) if method.sig.ident == "run"));
    let supertrait = if async_run {
        expand_async(&self_ty, &run, sampled, &apis)
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

fn expand_async(self_ty: &Type, run: &syn::ImplItemFn, sampled: bool, apis: &[ApiBinding]) -> TokenStream2 {
    let block = &run.block;
    let input = &run.sig.inputs[0];
    let env_ident = env_ident(&run.sig.inputs[1]);
    let env_ty = owned_env_type(&run.sig.inputs[1]);
    let ReturnType::Type(_, output) = &run.sig.output else {
        unreachable!("parse requires an async run's return type");
    };
    let target_checks = apis.iter().map(|ApiBinding { ty, name, .. }| {
        quote! {
            const _: () = ::aether_bloomery_program::__macro_internals::check_target::<
                #ty,
                ::aether_bloomery_program::__macro_internals::api_target::#name,
            >();
        }
    });
    let pure_checks = apis.iter().filter(|_| !sampled).map(|ApiBinding { ty, .. }| {
        quote! {
            const _: () = ::aether_bloomery_program::__macro_internals::RejectSampledOnPure::<
                { <#ty as ::aether_bloomery_program::InjectedApi>::SAMPLED },
            >::OK;
        }
    });
    let bindings = apis.iter().map(|ApiBinding { ident, ty, .. }| {
        quote! {
            let mut #ident = <#ty as ::aether_bloomery_program::InjectedApi>::from_env(&mut #env_ident);
        }
    });
    quote! {
        #(#target_checks)*
        #(#pure_checks)*
        impl ::aether_bloomery_program::AsyncProgram for #self_ty {
            fn run(
                #input,
                mut #env_ident: #env_ty,
            ) -> impl ::core::future::Future<Output = #output> + Send + 'static {
                async move {
                    #(#bindings)*
                    #block
                }
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
