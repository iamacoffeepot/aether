//! The reactor role's piece of the generated bundle root.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use aether_bloomery_kinds::REACTORS_SECTION;

use super::fnv1a_64;
use super::root::RolePieces;
use crate::classify::ReactorEntry;

pub fn pieces(reactors: &[ReactorEntry]) -> RolePieces {
    let reactor = quote! { ::aether_bloomery_bundle::__macro_internals::aether_bloomery_reactor };
    let mut list = quote! { #reactor::Nil };
    for entry in reactors.iter().rev() {
        let ty = &entry.ty;
        list = quote! { (#ty, #list) };
    }
    let field = quote! {
        reactors: #reactor::Root<#list>,
    };
    let init = quote! {
        let reactors = match #reactor::Root::new() {
            ::core::result::Result::Ok(inner) => inner,
            ::core::result::Result::Err(reason) => {
                return ::core::result::Result::Err(::aether_actor::ActorInitError::new(
                    #reactor::__macro_internals::ToString::to_string(reason.as_str()),
                ));
            }
        };
    };
    let handlers = expand_handlers(&reactor);
    let sections = reactors.iter().map(|entry| expand_section(entry, &reactor));
    let items = quote! {
        #(#sections)*
    };
    RolePieces { field_name: format_ident!("reactors"), field, init, handlers, items, spawns: Vec::new() }
}

fn expand_handlers(reactor: &TokenStream2) -> TokenStream2 {
    quote! {
        #[handler::manual]
        fn on_warm(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            warm: #reactor::kinds::Warm,
        ) {
            use ::aether_actor::OutboundReply;
            if ctx.reply_target().is_none() {
                ::aether_actor::__macro_internals::tracing::warn!(
                    "reactor root ignored a request with no reply target"
                );
                return;
            }
            ctx.reply(&self.reactors.warm(warm));
        }

        #[handler::manual]
        fn on_event(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            event: #reactor::kinds::Event,
        ) {
            use ::aether_actor::OutboundReply;
            if ctx.reply_target().is_none() {
                ::aether_actor::__macro_internals::tracing::warn!(
                    "reactor root ignored a request with no reply target"
                );
                return;
            }
            ctx.reply(&self.reactors.event(event));
        }

        #[handler::manual]
        fn on_status(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            _query: #reactor::kinds::StatusQuery,
        ) {
            use ::aether_actor::OutboundReply;
            if ctx.reply_target().is_none() {
                ::aether_actor::__macro_internals::tracing::warn!(
                    "reactor root ignored a request with no reply target"
                );
                return;
            }
            ctx.reply(&self.reactors.status());
        }
    }
}

fn expand_section(entry: &ReactorEntry, reactor: &TokenStream2) -> TokenStream2 {
    let ty = &entry.ty;
    let key = format!("{}:{}", entry.namespace, quote!(#ty));
    let hash = fnv1a_64(key.as_bytes());
    let len_ident = format_ident!("__AETHER_BLOOMERY_REACTOR_SECTION_LEN_{hash:016X}");
    let bytes_ident = format_ident!("__AETHER_BLOOMERY_REACTOR_SECTION_BYTES_{hash:016X}");
    let section_ident = format_ident!("__AETHER_BLOOMERY_REACTOR_SECTION_{hash:016X}");
    quote! {
        const #len_ident: usize = <#ty as #reactor::Reactor>::DECLARATION.len();
        const #bytes_ident: [u8; #len_ident] =
            #reactor::__macro_internals::record_array::<#len_ident>(
                <#ty as #reactor::Reactor>::DECLARATION,
            );
        const _: &[u8] = &#bytes_ident;
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = #REACTORS_SECTION)]
        static #section_ident: [u8; #len_ident] = #bytes_ident;
    }
}
