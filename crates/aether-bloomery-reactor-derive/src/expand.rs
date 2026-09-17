//! Generate inherent rule methods and the `Reactor` impl.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, quote_spanned};
use syn::Ident;
use syn::spanned::Spanned;

use crate::parse::{Param, ReactorDef, Rule};
use crate::pattern::match_test;

pub fn expand(def: ReactorDef) -> TokenStream2 {
    let ReactorDef { attrs, self_ty, name_const, rules, helpers } = def;
    let inherent_rules = rules.iter().map(expand_rule_method);
    let visits = rules.iter().map(expand_visit);
    let arm_evals = rules.iter().map(expand_arm_eval);

    quote! {
        #(#attrs)*
        impl #self_ty {
            #(#inherent_rules)*
            #(#helpers)*
        }

        #(#attrs)*
        impl ::aether_bloomery_reactor::Reactor for #self_ty {
            #name_const

            fn visit_arms(visitor: &mut impl ::aether_bloomery_reactor::ArmVisitor) {
                #(#visits)*
            }

            fn evaluate(
                &self,
                owner: &mut ::aether_bloomery_reactor::Owner,
            ) -> Result<
                ::aether_bloomery_reactor::__macro_internals::Vec<::aether_bloomery_reactor::Intent>,
                ::aether_bloomery_reactor::PrepareError,
            > {
                let mut intents = ::aether_bloomery_reactor::__macro_internals::Vec::new();
                #(#arm_evals)*
                Ok(intents)
            }
        }
    }
}

fn expand_rule_method(rule: &Rule) -> TokenStream2 {
    let Rule { vis, attrs, ident, trigger_pat, trigger_ty, params, output_ty, body } = rule;
    let trigger = Ident::new("__aether_reactor_trigger", trigger_ty.span());
    let param_args = params.iter().map(|Param { ident, ty }| {
        quote_spanned! { ty.span() => #ident: #ty }
    });

    quote! {
        #(#attrs)*
        #vis fn #ident(&self, #trigger: #trigger_ty, #(#param_args),*) -> #output_ty {
            match #trigger {
                #trigger_pat => #body,
                #[allow(unreachable_patterns)]
                _ => ::core::unreachable!("reactor rule trigger pattern already matched"),
            }
        }
    }
}

fn expand_visit(rule: &Rule) -> TokenStream2 {
    let name = rule.ident.to_string();
    let trigger_ty = &rule.trigger_ty;
    let params_ty = params_type(&rule.params);
    let output_ty = &rule.output_ty;
    quote_spanned! { trigger_ty.span() =>
        visitor.visit::<#trigger_ty, #params_ty, #output_ty>(#name);
    }
}

fn expand_arm_eval(rule: &Rule) -> TokenStream2 {
    let ident = &rule.ident;
    let trigger_ty = &rule.trigger_ty;
    let output_ty = &rule.output_ty;
    let params_ty = params_type(&rule.params);
    let unpack = unpack_pat(&rule.params);
    let call_args = (0..rule.params.len()).map(param_binding);
    let test_pat = match_test(&rule.trigger_pat);

    quote_spanned! { trigger_ty.span() =>
        match owner.prepare::<#trigger_ty, #params_ty>() {
            Ok(None) => {}
            Ok(Some((__aether_reactor_trigger, #unpack))) => {
                if matches!(&__aether_reactor_trigger, #test_pat) {
                    let __aether_reactor_output = self.#ident(__aether_reactor_trigger, #(#call_args),*);
                    intents.push(::aether_bloomery_reactor::Intent::from_output::<#output_ty>(
                        &__aether_reactor_output,
                    ));
                }
            }
            Err(error) if error.is_unknown_trigger() => {}
            Err(error) => return Err(error),
        }
    }
}

fn params_type(params: &[Param]) -> TokenStream2 {
    let mut rest = quote! { ::aether_bloomery_reactor::Nil };
    for param in params.iter().rev() {
        let ty = &param.ty;
        rest = quote_spanned! { ty.span() =>
            ::aether_bloomery_reactor::Arg::<_, #ty, #rest>
        };
    }
    rest
}

fn unpack_pat(params: &[Param]) -> TokenStream2 {
    let mut pat = quote! { () };
    for index in (0..params.len()).rev() {
        let ident = param_binding(index);
        pat = quote! { (#ident, #pat) };
    }
    pat
}

fn param_binding(index: usize) -> Ident {
    format_ident!("aether_reactor_param_{index}", span = proc_macro2::Span::mixed_site())
}
