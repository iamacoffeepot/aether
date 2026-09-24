//! The program role's piece of the generated bundle root.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::Ident;

use aether_bloomery_kinds::{BUNDLE_NAMESPACE, PROGRAMS_SECTION};

use super::fnv1a_64;
use super::root::RolePieces;
use crate::classify::ProgramEntry;

const INVOCATION_IDENT: &str = "__AetherBloomeryBundleInvocation";
const STATE_IDENT: &str = "__AetherBloomeryBundleProgramState";
const TABLE_IDENT: &str = "__AETHER_BLOOMERY_BUNDLE_PROGRAM_TABLE";

/// The per-call invocation actor the program role spawns inline, which the
/// generator lists as private so `export!` marks it rebuildable.
pub fn invocation_ident() -> Ident {
    format_ident!("{INVOCATION_IDENT}")
}

pub fn pieces(root: &Ident, programs: &[ProgramEntry]) -> RolePieces {
    let program = quote! { ::aether_bloomery_bundle::__macro_internals::aether_bloomery_program };
    let table = format_ident!("{TABLE_IDENT}");
    let invocation = invocation_ident();
    let state = format_ident!("{STATE_IDENT}");
    let field = quote! {
        programs: #state,
    };
    let init = quote! {
        let programs = #state {
            root: #program::Root::new(&#table),
            invokers: #program::__macro_internals::BTreeMap::new(),
            fetches: #program::__macro_internals::BTreeMap::new(),
        };
    };
    let handlers = expand_handlers(root, &invocation, &program);
    let state_struct = expand_state(&state, &program);
    let table_static = expand_table(&table, programs, &program);
    let invocation_actor = expand_invocation(root, &invocation, &table, &program, programs);
    let sections = programs.iter().map(|entry| expand_section(entry, &program));
    let items = quote! {
        #state_struct
        #table_static
        #invocation_actor
        #(#sections)*
    };
    RolePieces { field_name: format_ident!("programs"), field, init, handlers, items, spawns: vec![invocation_ident()] }
}

/// The program role's root state: the live-seq table plus the two relay maps
/// a fetch-on-miss travels through. An invocation's fetch goes to its root,
/// which sends it to whoever sent that invocation's `Invoke` (the driver) and
/// relays the answer back to the invocation.
fn expand_state(state: &Ident, program: &TokenStream2) -> TokenStream2 {
    quote! {
        struct #state {
            root: #program::Root<::core::option::Option<::aether_actor::ReplyHandle>>,
            /// Each live invocation's `Invoke` sender, keyed by the invocation.
            invokers: #program::__macro_internals::BTreeMap<
                ::aether_actor::ErasedActorRef,
                ::aether_actor::ErasedActorRef,
            >,
            /// The invocation each relayed fetch answers to, keyed by the
            /// root's own request.
            fetches: #program::__macro_internals::BTreeMap<
                #program::__macro_internals::RequestId,
                ::aether_actor::ErasedActorRef,
            >,
        }
    }
}

fn expand_handlers(root: &Ident, invocation: &Ident, program: &TokenStream2) -> TokenStream2 {
    quote! {
        #[handler::manual]
        fn on_invoke(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            invoke: #program::Invoke,
        ) {
            use ::aether_actor::OutboundReply;
            match self.programs.root.admit(&invoke) {
                ::core::result::Result::Err(rejected) => {
                    if ctx.reply_target().is_some() {
                        ctx.reply(&rejected);
                    }
                }
                ::core::result::Result::Ok(admission) => {
                    let seq = admission.seq();
                    let seq_name =
                        #program::__macro_internals::ToString::to_string(&seq);
                    match ctx.spawn_inline_child::<#root, #invocation>(
                        ::aether_actor::Subname::Named(&seq_name),
                        &(),
                    ) {
                        ::core::result::Result::Ok(child) => {
                            admission.start(child.id(), ctx.reply_target());
                            if let Some(invoker) = ctx.sender() {
                                self.programs.invokers.insert(child.erase(), invoker);
                            }
                            child.send(ctx, &invoke);
                        }
                        ::core::result::Result::Err(_) => {
                            let rejected = admission.spawn_failed();
                            if ctx.reply_target().is_some() {
                                ctx.reply(&rejected);
                            }
                        }
                    }
                }
            }
        }

        #[handler::manual]
        fn on_invoked(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            invoked: #program::Invoked,
        ) {
            use ::aether_actor::OutboundReply;
            let Some(sender) = ctx.sender() else {
                return;
            };
            let Some((_, reply)) = self.programs.root.finish(&invoked, Some(sender.id())) else {
                return;
            };
            self.programs.invokers.remove(&sender);
            if let Some(reply) = reply {
                ctx.reply_to(reply, &invoked);
            }
            ctx.despawn_inline_child(sender);
        }

        #[handler::manual]
        fn on_read_artifact(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            request: #program::kinds::ReadArtifact,
        ) {
            use ::aether_actor::{MailSender, OutboundReply};
            let Some(sender) = ctx.sender() else {
                return;
            };
            let Some(invoker) = self.programs.invokers.get(&sender).copied() else {
                let refused = #program::kinds::ReadArtifactResult::Err {
                    digest: request.digest,
                    message: #program::__macro_internals::ToString::to_string("no live invocation sent this fetch"),
                };
                if ctx.reply_target().is_some() {
                    ctx.reply(&refused);
                } else {
                    ctx.send_to(sender, &refused);
                }
                return;
            };
            ctx.send_to(invoker, &request);
            let fetch = #program::__macro_internals::RequestId(ctx.prev_correlation());
            self.programs.fetches.insert(fetch, sender);
        }

        #[handler::manual]
        fn on_read_artifact_result(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
            result: #program::kinds::ReadArtifactResult,
        ) {
            let Some(fetch) = ctx.in_reply_to() else {
                return;
            };
            let Some(invocation) = self.programs.fetches.remove(&fetch) else {
                return;
            };
            ctx.send_to(invocation, &result);
        }
    }
}

fn expand_table(table: &Ident, programs: &[ProgramEntry], program: &TokenStream2) -> TokenStream2 {
    let entries = programs.iter().map(|entry| {
        let ty = &entry.ty;
        if entry.meta.async_run {
            quote! { #program::ProgramEntry::of_async::<#ty>() }
        } else {
            quote! { #program::ProgramEntry::of::<#ty>() }
        }
    });
    quote! {
        static #table: #program::ProgramTable =
            #program::__macro_internals::program_table(&[#(#entries),*]);
    }
}

fn expand_invocation(
    root: &Ident,
    invocation: &Ident,
    table: &Ident,
    program: &TokenStream2,
    programs: &[ProgramEntry],
) -> TokenStream2 {
    let namespace = format!("{BUNDLE_NAMESPACE}.invocation");
    let api_tys: Vec<_> = programs.iter().flat_map(|entry| entry.meta.apis.iter()).collect();
    let resume = resume_after_poll(program);
    let send_pending = expand_send_pending(program, api_tys.as_slice());
    let fetch_reply = expand_fetch_reply(program);
    quote! {
        struct #invocation {
            session: ::core::option::Option<#program::AsyncSession>,
            parent: ::core::option::Option<::aether_actor::ErasedActorRef>,
            waiting: #program::__macro_internals::BTreeMap<
                #program::__macro_internals::RequestId,
                #program::__macro_internals::PendingCall,
            >,
            fetching: #program::__macro_internals::BTreeMap<
                #program::kinds::Digest,
                #program::__macro_internals::PendingArtifact,
            >,
        }

        #[::aether_actor::actor(instanced, child_of(#root))]
        impl ::aether_actor::WasmActor for #invocation {
            const NAMESPACE: &'static str = #namespace;

            fn init(
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                Ok(Self {
                    session: ::core::option::Option::None,
                    parent: ::core::option::Option::None,
                    waiting: #program::__macro_internals::BTreeMap::new(),
                    fetching: #program::__macro_internals::BTreeMap::new(),
                })
            }

            #[handler::manual]
            fn on_invoke(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Manual>,
                invoke: #program::Invoke,
            ) {
                self.parent = ctx.sender();
                match #program::__macro_internals::start_invocation(&#table, invoke) {
                    #program::__macro_internals::Started::Finished(invoked) => {
                        self.reply_invoked(ctx, &invoked);
                    }
                    #program::__macro_internals::Started::Live { session, waiting } => {
                        self.session = ::core::option::Option::Some(session);
                        if let Some(pending) = waiting {
                            self.send_pending(ctx, pending);
                        }
                    }
                }
            }

            #fetch_reply

            #[fallback]
            fn on_mail(&mut self, ctx: &mut ::aether_actor::WasmCtx<'_>, mail: ::aether_actor::Mail<'_>) {
                let Some(request) = ctx.in_reply_to() else {
                    return;
                };
                let Some(pending) = self.waiting.remove(&request) else {
                    return;
                };
                if mail.kind() != pending.expected_reply {
                    self.waiting.insert(request, pending);
                    return;
                }
                let Some(session) = self.session.as_mut() else {
                    return;
                };
                session.fulfill_send(&pending, mail.kind(), mail.bytes().to_vec());
                #resume
            }
        }

        impl #invocation {
            #send_pending

            fn reply_invoked<A, M: ::aether_actor::ReplyMode>(
                &self,
                ctx: &mut ::aether_actor::WasmCtx<'_, A, M>,
                invoked: &#program::Invoked,
            ) {
                if let Some(parent) = self.parent {
                    ctx.send_to(parent, invoked);
                }
            }
        }
    }
}

/// The invocation's handler for its root's relay of a fetch-on-miss answer.
fn expand_fetch_reply(program: &TokenStream2) -> TokenStream2 {
    let resume = resume_after_poll(program);
    quote! {
        /// The root's relay of this invocation's fetch-on-miss answer. It
        /// arrives as a cluster-local send from the parent, which carries
        /// no correlation, so the wait is keyed by the fetched digest.
        #[handler::single]
        fn on_read_artifact_result(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_>,
            result: #program::kinds::ReadArtifactResult,
        ) {
            if self.parent.is_none() || ctx.sender() != self.parent {
                return;
            }
            let digest = match &result {
                #program::kinds::ReadArtifactResult::Found { digest, .. }
                | #program::kinds::ReadArtifactResult::Missing { digest }
                | #program::kinds::ReadArtifactResult::Err { digest, .. } => *digest,
            };
            let Some(pending) = self.fetching.remove(&digest) else {
                return;
            };
            let Some(session) = self.session.as_mut() else {
                return;
            };
            session.fulfill(pending, result);
            #resume
        }
    }
}

fn resume_after_poll(program: &TokenStream2) -> TokenStream2 {
    quote! {
        match session.poll() {
            #program::__macro_internals::PollResult::Finished(invoked) => {
                self.session = ::core::option::Option::None;
                self.waiting.clear();
                self.fetching.clear();
                self.reply_invoked(ctx, &invoked);
            }
            #program::__macro_internals::PollResult::NeedArtifact(pending) => {
                self.send_pending(ctx, #program::__macro_internals::Pending::Artifact(pending));
            }
            #program::__macro_internals::PollResult::NeedSend(pending) => {
                self.send_pending(ctx, #program::__macro_internals::Pending::Send(pending));
            }
            #program::__macro_internals::PollResult::Waiting => {}
        }
    }
}

fn expand_send_pending(program: &TokenStream2, api_tys: &[&syn::Type]) -> TokenStream2 {
    let resume = resume_after_poll(program);
    quote! {
        fn send_pending<A, M: ::aether_actor::ReplyMode>(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, A, M>,
            pending: #program::__macro_internals::Pending,
        ) {
            use ::aether_actor::MailSender;
            match pending {
                #program::__macro_internals::Pending::Artifact(pending) => {
                    let Some(parent) = self.parent else {
                        if let Some(session) = self.session.as_mut() {
                            session.fulfill(pending, #program::kinds::ReadArtifactResult::Err {
                                digest: pending.digest,
                                message: #program::__macro_internals::ToString::to_string(
                                    "the invocation has no parent to fetch through",
                                ),
                            });
                            #resume
                        }
                        return;
                    };
                    ctx.send_to(parent, &#program::kinds::ReadArtifact { digest: pending.digest });
                    self.fetching.insert(pending.digest, pending);
                }
                #program::__macro_internals::Pending::Send(pending) => {
                    const ALLOWED: &[&str] = &[
                        #(<<#api_tys as #program::InjectedApi>::Target as ::aether_actor::Addressable>::NAMESPACE),*
                    ];
                    if !ALLOWED.iter().copied().any(|name| name == pending.mailbox) {
                        if let Some(session) = self.session.as_mut() {
                            session.reject_send(#program::Refusal::Refused {
                                reason: #program::kinds::Detail::new("mailbox is not in the program allowlist"),
                            });
                            #resume
                        }
                        return;
                    }
                    pending.dispatch(ctx);
                    let request = #program::__macro_internals::RequestId(ctx.prev_correlation());
                    self.waiting.insert(request, pending);
                }
            }
        }
    }
}

fn expand_section(entry: &ProgramEntry, program: &TokenStream2) -> TokenStream2 {
    let name = &entry.meta.name;
    let intent = &entry.meta.intent;
    let input = &entry.meta.input;
    let result = &entry.meta.result;
    let hash = fnv1a_64(name.value().as_bytes());
    let len_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_LEN_{hash:016X}");
    let bytes_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_BYTES_{hash:016X}");
    let section_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_SECTION_{hash:016X}");
    let mode = if entry.meta.sampled {
        quote! { #program::__macro_internals::MODE_SAMPLED }
    } else {
        quote! { #program::__macro_internals::MODE_PURE }
    };
    quote! {
        const #len_ident: usize = #program::__macro_internals::program_record_len(
            #name.as_bytes(),
            #intent.as_bytes(),
        );
        const #bytes_ident: [u8; #len_ident] = #program::__macro_internals::write_program_record::<#len_ident>(
            #name.as_bytes(),
            <#input as #program::__macro_internals::Kind>::ID.0,
            <#result as #program::__macro_internals::Kind>::ID.0,
            #mode,
            #intent.as_bytes(),
        );
        const _: &[u8] = &#bytes_ident;
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = #PROGRAMS_SECTION)]
        static #section_ident: [u8; #len_ident] = #bytes_ident;
    }
}
