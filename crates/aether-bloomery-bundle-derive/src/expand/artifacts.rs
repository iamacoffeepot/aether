//! Shared root dispatcher for program and view artifact fetches.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::root::RolePieces;

const STATE_IDENT: &str = "__AetherBloomeryBundleArtifactState";
const ROUTE_IDENT: &str = "__AetherBloomeryBundleArtifactRoute";
const REPLY_IDENT: &str = "__AetherBloomeryBundleReactorReply";

pub fn pieces(programs: bool, reactors: bool) -> RolePieces {
    let program = quote! { ::aether_bloomery_bundle::__macro_internals::aether_bloomery_program };
    let state = format_ident!("{STATE_IDENT}");
    let route = format_ident!("{ROUTE_IDENT}");
    let reply = format_ident!("{REPLY_IDENT}");
    let program_variant = programs.then(|| {
        quote! {
            Program {
                driver: ::aether_actor::ErasedActorRef,
                invocation: ::aether_actor::ErasedActorRef,
            },
        }
    });
    let view_variant = reactors.then(|| {
        quote! {
            View {
                driver: ::aether_actor::ErasedActorRef,
            },
        }
    });
    let program_driver = programs.then(|| quote! { Self::Program { driver, .. } => *driver, });
    let view_driver = reactors.then(|| quote! { Self::View { driver } => *driver, });
    let field = quote! {
        artifacts: #state,
    };
    let init = quote! {
        let artifacts = #state { routes: #program::__macro_internals::BTreeMap::new() };
    };
    let handlers = expand_handler(programs, reactors, &program, &route, &reply);
    let items = quote! {
        struct #state {
            routes: #program::__macro_internals::BTreeMap<
                #program::__macro_internals::RequestId,
                #route,
            >,
        }

        enum #route {
            #program_variant
            #view_variant
        }

        impl #route {
            fn driver(&self) -> ::aether_actor::ErasedActorRef {
                match self {
                    #program_driver
                    #view_driver
                }
            }
        }
    };
    RolePieces { field_name: format_ident!("artifacts"), field, init, handlers, items, spawns: Vec::new() }
}

fn expand_handler(
    programs: bool,
    reactors: bool,
    program: &TokenStream2,
    route: &syn::Ident,
    reply: &syn::Ident,
) -> TokenStream2 {
    let program_arm = programs.then(|| {
        quote! {
            #route::Program { invocation, .. } => {
                ctx.send_to(invocation, &result);
            }
        }
    });
    let view_arm = reactors.then(|| {
        let reactor = quote! { ::aether_bloomery_bundle::__macro_internals::aether_bloomery_reactor };
        quote! {
            #route::View { driver } => {
                let Some(polled) = self.reactors.root.fulfill(result) else {
                    return;
                };
                match polled {
                    #reactor::RootPoll::NeedArtifact(artifact) => {
                        ctx.send_to(driver, &#reactor::kinds::ReadArtifact { digest: artifact.digest });
                        let request = #program::__macro_internals::RequestId(ctx.prev_correlation());
                        self.artifacts.routes.insert(request, #route::View { driver });
                    }
                    #reactor::RootPoll::Complete(completion) => {
                        let held = ::core::mem::replace(&mut self.reactors.reply, #reply::Idle);
                        match (held, completion) {
                            (#reply::Warm(held), #reactor::Completion::Warmed(warmed)) => {
                                held.answer(ctx, &warmed);
                            }
                            (#reply::Event(held), #reactor::Completion::Evaluated(evaluated)) => {
                                held.answer(ctx, &evaluated);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    });
    quote! {
        #[handler::manual]
        fn on_read_artifact_result(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            result: #program::kinds::ReadArtifactResult,
        ) {
            use ::aether_actor::{MailSender, OutboundReply};
            let Some(request) = ctx.in_reply_to() else {
                return;
            };
            let Some(sender) = ctx.sender() else {
                return;
            };
            let Some(route) = self.artifacts.routes.get(&request) else {
                return;
            };
            if route.driver() != sender {
                return;
            }
            let Some(route) = self.artifacts.routes.remove(&request) else {
                return;
            };
            match route {
                #program_arm
                #view_arm
            }
        }
    }
}
