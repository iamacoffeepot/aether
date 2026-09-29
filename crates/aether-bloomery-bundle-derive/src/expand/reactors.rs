//! The reactor role's piece of the generated bundle root.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use aether_bloomery_kinds::REACTORS_SECTION;

use super::fnv1a_64;
use super::root::RolePieces;
use crate::classify::ReactorEntry;

const STATE_IDENT: &str = "__AetherBloomeryBundleReactorState";
const REPLY_IDENT: &str = "__AetherBloomeryBundleReactorReply";

pub fn pieces(reactors: &[ReactorEntry]) -> RolePieces {
    let reactor = quote! { ::aether_bloomery_bundle::__macro_internals::aether_bloomery_reactor };
    let state = format_ident!("{STATE_IDENT}");
    let reply = format_ident!("{REPLY_IDENT}");
    let mut list = quote! { #reactor::Nil };
    for entry in reactors.iter().rev() {
        let ty = &entry.ty;
        list = quote! { (#ty, #list) };
    }
    let field = quote! {
        reactors: #state<#list>,
    };
    let init = quote! {
        let reactors = match #reactor::Root::new() {
            ::core::result::Result::Ok(root) => #state {
                root,
                reply: #reply::Idle,
            },
            ::core::result::Result::Err(reason) => {
                return ::core::result::Result::Err(::aether_actor::ActorInitError::new(
                    #reactor::__macro_internals::ToString::to_string(reason.as_str()),
                ));
            }
        };
    };
    let handlers = expand_handlers(&reactor, &reply);
    let sections = reactors.iter().map(|entry| expand_section(entry, &reactor));
    let items = quote! {
        struct #state<L: #reactor::ReactorList> {
            root: #reactor::Root<L>,
            reply: #reply,
        }

        enum #reply {
            Idle,
            Warm(::aether_actor::Held<#reactor::kinds::Warmed>),
            Event(::aether_actor::Held<#reactor::kinds::Evaluated>),
        }

        #(#sections)*
    };
    RolePieces { field_name: format_ident!("reactors"), field, init, handlers, items, spawns: Vec::new() }
}

fn expand_handlers(reactor: &TokenStream2, reply: &syn::Ident) -> TokenStream2 {
    let requests = expand_request_handlers(reactor, reply);
    let status = expand_status_handler(reactor);
    quote! {
        #requests
        #status
    }
}

fn expand_request_handlers(reactor: &TokenStream2, reply: &syn::Ident) -> TokenStream2 {
    let program = quote! { ::aether_bloomery_bundle::__macro_internals::aether_bloomery_program };
    let no_driver = expand_no_driver_result(reactor);
    quote! {
        #[handler::single]
        fn on_warm(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased>,
            warm: #reactor::kinds::Warm,
        ) -> ::aether_actor::Pending<#reactor::kinds::Warmed> {
            use ::aether_actor::MailSender;
            let (pending, held) = ctx.hold::<#reactor::kinds::Warmed>();
            if held.__is_detached() {
                ::aether_actor::__macro_internals::tracing::warn!(
                    "reactor root ignored a request with no reply target"
                );
                drop(held);
                return pending;
            }
            let driver = ctx.sender();
            match self.reactors.root.start_warm(warm) {
                #reactor::RootPoll::Complete(#reactor::Completion::Warmed(warmed)) => {
                    held.answer(ctx, &warmed);
                }
                #reactor::RootPoll::Complete(#reactor::Completion::Evaluated(_)) => {
                    ::core::unreachable!();
                }
                #reactor::RootPoll::NeedArtifact(artifact) => {
                    if let ::core::option::Option::Some(driver) = driver {
                        self.reactors.reply = #reply::Warm(held);
                        ctx.send_to(driver, &#reactor::kinds::ReadArtifact { digest: artifact.digest });
                        let request = #program::__macro_internals::RequestId(ctx.prev_correlation());
                        self.artifacts.routes.insert(
                            request,
                            __AetherBloomeryBundleArtifactRoute::View { driver },
                        );
                    } else {
                        let result = #no_driver;
                        let ::core::option::Option::Some(#reactor::RootPoll::Complete(
                            #reactor::Completion::Warmed(warmed),
                        )) = self.reactors.root.fulfill(result) else {
                            ::core::unreachable!();
                        };
                        held.answer(ctx, &warmed);
                    }
                }
            }
            pending
        }

        #[handler::single]
        fn on_event(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased>,
            event: #reactor::kinds::Event,
        ) -> ::aether_actor::Pending<#reactor::kinds::Evaluated> {
            use ::aether_actor::MailSender;
            let (pending, held) = ctx.hold::<#reactor::kinds::Evaluated>();
            if held.__is_detached() {
                ::aether_actor::__macro_internals::tracing::warn!(
                    "reactor root ignored a request with no reply target"
                );
                drop(held);
                return pending;
            }
            let driver = ctx.sender();
            match self.reactors.root.start_event(event) {
                #reactor::RootPoll::Complete(#reactor::Completion::Evaluated(evaluated)) => {
                    held.answer(ctx, &evaluated);
                }
                #reactor::RootPoll::Complete(#reactor::Completion::Warmed(_)) => {
                    ::core::unreachable!();
                }
                #reactor::RootPoll::NeedArtifact(artifact) => {
                    if let ::core::option::Option::Some(driver) = driver {
                        self.reactors.reply = #reply::Event(held);
                        ctx.send_to(driver, &#reactor::kinds::ReadArtifact { digest: artifact.digest });
                        let request = #program::__macro_internals::RequestId(ctx.prev_correlation());
                        self.artifacts.routes.insert(
                            request,
                            __AetherBloomeryBundleArtifactRoute::View { driver },
                        );
                    } else {
                        let result = #no_driver;
                        let ::core::option::Option::Some(#reactor::RootPoll::Complete(
                            #reactor::Completion::Evaluated(evaluated),
                        )) = self.reactors.root.fulfill(result) else {
                            ::core::unreachable!();
                        };
                        held.answer(ctx, &evaluated);
                    }
                }
            }
            pending
        }
    }
}

fn expand_no_driver_result(reactor: &TokenStream2) -> TokenStream2 {
    quote! {
        #reactor::kinds::ReadArtifactResult::Err {
            digest: artifact.digest,
            message: #reactor::__macro_internals::ToString::to_string(
                "reactor artifact read has no driver return path",
            ),
        }
    }
}

fn expand_status_handler(reactor: &TokenStream2) -> TokenStream2 {
    quote! {
        #[handler::single]
        fn on_status(
            &mut self,
            _ctx: &mut ::aether_actor::WasmCtx<'_>,
            _query: #reactor::kinds::StatusQuery,
        ) -> #reactor::kinds::Status {
            self.reactors.root.status()
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
