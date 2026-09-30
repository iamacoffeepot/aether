//! The program role's piece of the generated bundle root.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::Ident;

use aether_bloomery_kinds::{BUNDLE_NAMESPACE, PROGRAMS_SECTION};

use super::fnv1a_64;
use super::root::RolePieces;
use crate::classify::ProgramEntry;

const INVOCATION_IDENT: &str = "__AetherBloomeryBundleInvocation";
const LIVE_IDENT: &str = "__AetherBloomeryBundleLiveInvocation";
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
    let live = format_ident!("{LIVE_IDENT}");
    let field = quote! {
        programs: #state,
    };
    let init = quote! {
        let programs = #state {
            root: #program::Root::new(&#table),
            invocations: #program::__macro_internals::BTreeMap::new(),
            fetches: #program::__macro_internals::BTreeMap::new(),
            calls: #program::__macro_internals::BTreeMap::new(),
        };
    };
    let handlers = expand_handlers(root, &invocation, &live, &program);
    let state_struct = expand_state(&state, &live, &invocation, &program);
    let table_static = expand_table(&table, programs, &program);
    let invocation_actor = expand_invocation(root, &invocation, &table, &program);
    let sections = programs.iter().map(|entry| expand_section(entry, &program));
    let items = quote! {
        #state_struct
        #table_static
        #invocation_actor
        #(#sections)*
    };
    RolePieces { field_name: format_ident!("programs"), field, init, handlers, items, spawns: vec![invocation_ident()] }
}

/// The program role's root state: the live-seq table, each live seq holding
/// the `Invoked` reply its `Invoke` is owed (ADR-0243), plus the relay maps a
/// fetch-on-miss and a program API call travel through. An invocation's fetch
/// or API call goes to its root, which sends it to whoever sent that
/// invocation's `Invoke` (the driver) and relays the answer back to the
/// invocation (ADR-0240 D6). The root keeps each invocation as the typed
/// child its spawn returned and each invoker as a `ProgramInvoker`, cast once
/// when the `Invoke` arrives (ADR-0231 §4), so every relay sends through a
/// typed reference.
fn expand_state(state: &Ident, live: &Ident, invocation: &Ident, program: &TokenStream2) -> TokenStream2 {
    quote! {
        struct #state {
            root: #program::Root<::aether_actor::Held<#program::Invoked>>,
            /// Each live invocation, keyed by its erased reference, the
            /// sender its relays and its `Invoked` report arrive from.
            invocations: #program::__macro_internals::BTreeMap<::aether_actor::ErasedActorRef, #live>,
            /// The invocation each relayed fetch answers to, keyed by the
            /// root's own request.
            fetches: #program::__macro_internals::BTreeMap<
                #program::__macro_internals::RequestId,
                ::aether_actor::InlineChild<#invocation>,
            >,
            /// The invocation each relayed API call answers to, keyed by the
            /// root's own request.
            calls: #program::__macro_internals::BTreeMap<
                #program::__macro_internals::RequestId,
                ::aether_actor::InlineChild<#invocation>,
            >,
        }

        /// One live invocation: the child the root spawned, and the
        /// `ProgramInvoker` its `Invoke` came from, `None` when that sender
        /// is not one (a harness rather than a driver), so its relays are
        /// refused.
        #[derive(Clone, Copy)]
        struct #live {
            child: ::aether_actor::InlineChild<#invocation>,
            invoker: ::core::option::Option<::aether_actor::ProtocolRef<#program::kinds::ProgramInvoker>>,
        }
    }
}

fn expand_handlers(root: &Ident, invocation: &Ident, live: &Ident, program: &TokenStream2) -> TokenStream2 {
    let relays = expand_relay_handlers(live, program);
    quote! {
        /// Admit `invoke` and run it on a per-seq invocation child. The
        /// `Invoked` reply is held: a rejection answers it at once, and a
        /// started seq keeps it live until its child reports back.
        #[handler::single]
        fn on_invoke(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased>,
            invoke: #program::Invoke,
        ) -> ::aether_actor::Pending<#program::Invoked> {
            let (pending, held) = ctx.hold::<#program::Invoked>();
            match self.programs.root.admit(&invoke) {
                ::core::result::Result::Err(rejected) => held.answer(ctx, &rejected),
                ::core::result::Result::Ok(admission) => {
                    let seq = admission.seq();
                    let seq_name =
                        #program::__macro_internals::ToString::to_string(&seq);
                    match ctx.spawn_inline_child::<#root, #invocation>(
                        ::aether_actor::Subname::Named(&seq_name),
                        &(),
                    ) {
                        ::core::result::Result::Ok(child) => {
                            admission.start(child.id(), held);
                            let invoker = ctx
                                .sender()
                                .and_then(|sender| ctx.cast::<#program::kinds::ProgramInvoker>(sender));
                            self.programs.invocations.insert(child.erase(), #live { child, invoker });
                            child.send(ctx, &invoke);
                        }
                        ::core::result::Result::Err(_) => held.answer(ctx, &admission.spawn_failed()),
                    }
                }
            }
            pending
        }

        /// A child's `Invoked` report: answer the seq's held reply and
        /// retire the child.
        #[handler::single]
        fn on_invoked(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased>,
            invoked: #program::Invoked,
        ) {
            let Some(sender) = ctx.sender() else {
                return;
            };
            let Some((_, held)) = self.programs.root.finish(&invoked, Some(sender.id())) else {
                return;
            };
            self.programs.invocations.remove(&sender);
            held.answer(ctx, &invoked);
            ctx.despawn_inline_child(sender);
        }

        #relays
    }
}

/// The root's relay handlers: an invocation's fetch-on-miss and program API
/// call go to the `ProgramInvoker` its `Invoke` came from, and each answer
/// returns to the invocation that asked, keyed by the root's own request
/// (ADR-0240 D6). A live invocation whose `Invoke` came from no invoker is
/// refused through its typed child. A sender that is no live invocation is
/// refused through the reply when it asked for one; otherwise nothing typed
/// names it and no reply is owed, so the request is dropped with a warning.
fn expand_relay_handlers(live: &Ident, program: &TokenStream2) -> TokenStream2 {
    quote! {
        #[handler::unchecked(reason = "relays the request (ADR-0243 §8)")]
        fn on_read_artifact(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Unchecked>,
            request: #program::kinds::ReadArtifact,
        ) {
            use ::aether_actor::{MailSender, OutboundReply};
            let refused = |message: &str| #program::kinds::ReadArtifactResult::Err {
                digest: request.digest,
                message: #program::__macro_internals::ToString::to_string(message),
            };
            let live = ctx.sender().and_then(|sender| self.programs.invocations.get(&sender).copied());
            match live {
                ::core::option::Option::Some(#live { child, invoker: ::core::option::Option::Some(invoker) }) => {
                    ctx.send_to(invoker, &request);
                    let fetch = #program::__macro_internals::RequestId(ctx.prev_correlation());
                    self.programs.fetches.insert(fetch, child);
                }
                ::core::option::Option::Some(#live { child, invoker: ::core::option::Option::None }) => {
                    child.send(ctx, &refused("the invocation's Invoke came from no program invoker"));
                }
                ::core::option::Option::None if ctx.reply_target().is_some() => {
                    ctx.reply(&refused("no live invocation sent this fetch"));
                }
                ::core::option::Option::None => {
                    ::aether_actor::__macro_internals::tracing::warn!(
                        "program root dropped a fetch no live invocation sent"
                    );
                }
            }
        }

        #[handler::single]
        fn on_read_artifact_result(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased>,
            result: #program::kinds::ReadArtifactResult,
        ) {
            let Some(fetch) = ctx.in_reply_to() else {
                return;
            };
            let Some(invocation) = self.programs.fetches.remove(&fetch) else {
                return;
            };
            invocation.send(ctx, &result);
        }

        #[handler::unchecked(reason = "relays the request (ADR-0243 §8)")]
        fn on_api_call(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased, ::aether_actor::Unchecked>,
            request: #program::kinds::ApiCall,
        ) {
            use ::aether_actor::{MailSender, OutboundReply};
            let refused = |reason: &str| #program::kinds::ApiCallResult::Refused {
                call: request.call,
                refusal: #program::Refusal::Refused { reason: #program::kinds::Detail::new(reason) },
            };
            let live = ctx.sender().and_then(|sender| self.programs.invocations.get(&sender).copied());
            match live {
                ::core::option::Option::Some(#live { child, invoker: ::core::option::Option::Some(invoker) }) => {
                    ctx.send_to(invoker, &request);
                    let call = #program::__macro_internals::RequestId(ctx.prev_correlation());
                    self.programs.calls.insert(call, child);
                }
                ::core::option::Option::Some(#live { child, invoker: ::core::option::Option::None }) => {
                    child.send(ctx, &refused("the invocation's Invoke came from no program invoker"));
                }
                ::core::option::Option::None if ctx.reply_target().is_some() => {
                    ctx.reply(&refused("no live invocation sent this call"));
                }
                ::core::option::Option::None => {
                    ::aether_actor::__macro_internals::tracing::warn!(
                        "program root dropped an API call no live invocation sent"
                    );
                }
            }
        }

        #[handler::single]
        fn on_api_call_result(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Erased>,
            result: #program::kinds::ApiCallResult,
        ) {
            let Some(call) = ctx.in_reply_to() else {
                return;
            };
            let Some(invocation) = self.programs.calls.remove(&call) else {
                return;
            };
            invocation.send(ctx, &result);
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

fn expand_invocation(root: &Ident, invocation: &Ident, table: &Ident, program: &TokenStream2) -> TokenStream2 {
    let namespace = format!("{BUNDLE_NAMESPACE}.invocation");
    let send_pending = expand_send_pending(program);
    let fetch_reply = expand_fetch_reply(program);
    let api_reply = expand_api_reply(program);
    quote! {
        struct #invocation {
            session: ::core::option::Option<#program::AsyncSession>,
            /// The root that spawned this invocation, cast once from its
            /// `Invoke`'s sender; its fetches, API calls, and `Invoked`
            /// report go through it.
            parent: ::core::option::Option<::aether_actor::ProtocolRef<#program::kinds::ProgramRelay>>,
            /// Each relayed API call awaiting its answer, keyed by the call
            /// id this invocation minted.
            waiting: #program::__macro_internals::BTreeMap<u64, #program::__macro_internals::PendingCall>,
            next_call: u64,
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
                    next_call: 0,
                    fetching: #program::__macro_internals::BTreeMap::new(),
                })
            }

            #[handler::single]
            fn on_invoke(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_>,
                invoke: #program::Invoke,
            ) {
                self.parent = ctx.sender().and_then(|sender| ctx.cast::<#program::kinds::ProgramRelay>(sender));
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

            #api_reply
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
            if self.parent.is_none() || ctx.sender() != self.parent.map(::aether_actor::ProtocolRef::erase) {
                return;
            }
            let digest = match &result {
                #program::kinds::ReadArtifactResult::Found { artifact } => artifact.claimed().unverified(),
                #program::kinds::ReadArtifactResult::Missing { digest }
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

/// The invocation's handler for its root's relay of an API call's answer.
fn expand_api_reply(program: &TokenStream2) -> TokenStream2 {
    let resume = resume_after_poll(program);
    quote! {
        /// The root's relay of the driver's answer to one of this
        /// invocation's API calls. Like a fetch answer it arrives as a
        /// cluster-local send from the parent, so the wait is keyed by the
        /// call id the invocation minted.
        #[handler::single]
        fn on_api_call_result(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_>,
            result: #program::kinds::ApiCallResult,
        ) {
            if self.parent.is_none() || ctx.sender() != self.parent.map(::aether_actor::ProtocolRef::erase) {
                return;
            }
            let Some(pending) = self.waiting.remove(&result.call()) else {
                return;
            };
            let Some(session) = self.session.as_mut() else {
                return;
            };
            match result {
                #program::kinds::ApiCallResult::Replied { kind, payload, .. } => {
                    session.fulfill_send(&pending, kind, payload);
                }
                #program::kinds::ApiCallResult::Refused { refusal, .. } => session.reject_send(refusal),
            }
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

/// The invocation's pump for one pending wait. A fetch and a captured API call
/// both go to the parent root, which relays them to the driver; the
/// invocation declares no dependency and sends to nothing else.
fn expand_send_pending(program: &TokenStream2) -> TokenStream2 {
    let resume = resume_after_poll(program);
    quote! {
        fn send_pending<M: ::aether_actor::ReplyMode>(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, Self, M>,
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
                    let Some(parent) = self.parent else {
                        if let Some(session) = self.session.as_mut() {
                            session.reject_send(#program::Refusal::Refused {
                                reason: #program::kinds::Detail::new(
                                    "the invocation has no parent to relay its call through",
                                ),
                            });
                            #resume
                        }
                        return;
                    };
                    let call = self.next_call;
                    self.next_call = call.wrapping_add(1);
                    ctx.send_to(parent, &pending.api_call(call));
                    self.waiting.insert(call, pending);
                }
            }
        }
    }
}

fn expand_section(entry: &ProgramEntry, program: &TokenStream2) -> TokenStream2 {
    let name = &entry.meta.name;
    let intent = &entry.meta.intent;
    let ty = &entry.ty;
    let hash = fnv1a_64(name.value().as_bytes());
    let len_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_LEN_{hash:016X}");
    let bytes_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_BYTES_{hash:016X}");
    let section_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_SECTION_{hash:016X}");
    let mode = if entry.meta.sampled {
        quote! { #program::__macro_internals::MODE_SAMPLED }
    } else {
        quote! { #program::__macro_internals::MODE_PURE }
    };
    let apis = &entry.meta.apis;
    quote! {
        const #len_ident: usize = #program::__macro_internals::program_record_len(
            #name.as_bytes(),
            #intent.as_bytes(),
        );
        const #bytes_ident: [u8; #len_ident] = #program::__macro_internals::write_program_record::<#len_ident>(
            #name.as_bytes(),
            <<#ty as #program::Program>::Input as #program::__macro_internals::Kind>::ID.0,
            <<#ty as #program::Program>::Result as #program::__macro_internals::Kind>::ID.0,
            #mode,
            #program::__macro_internals::api_mask(&[#(#program::kinds::ProgramApi::#apis),*]),
            #intent.as_bytes(),
        );
        const _: &[u8] = &#bytes_ident;
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = #PROGRAMS_SECTION)]
        static #section_ident: [u8; #len_ident] = #bytes_ident;
    }
}
