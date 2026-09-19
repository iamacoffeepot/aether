//! The program role's piece of the generated bundle root.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::Ident;

use aether_bloomery_kinds::{BUNDLE_NAMESPACE, PROGRAMS_SECTION};

use super::fnv1a_64;
use super::root::RolePieces;
use crate::classify::ProgramEntry;

const INVOCATION_IDENT: &str = "__AetherBloomeryBundleInvocation";
const TABLE_IDENT: &str = "__AETHER_BLOOMERY_BUNDLE_PROGRAM_TABLE";

pub fn pieces(root: &Ident, programs: &[ProgramEntry]) -> RolePieces {
    let program = quote! { ::aether_bloomery_bundle::__macro_internals::aether_bloomery_program };
    let table = format_ident!("{TABLE_IDENT}");
    let invocation = format_ident!("{INVOCATION_IDENT}");
    let field = quote! {
        programs: #program::Root<::core::option::Option<::aether_actor::ReplyHandle>>,
    };
    let init = quote! {
        let programs = #program::Root::new(&#table);
    };
    let handlers = expand_handlers(root, &invocation, &program);
    let table_static = expand_table(&table, programs, &program);
    let invocation_actor = expand_invocation(root, &invocation, &table, &program);
    let sections = programs.iter().map(|entry| expand_section(entry, &program));
    let items = quote! {
        #table_static
        #invocation_actor
        #(#sections)*
    };
    RolePieces { field_name: format_ident!("programs"), field, init, handlers, items }
}

fn expand_handlers(root: &Ident, invocation: &Ident, program: &TokenStream2) -> TokenStream2 {
    quote! {
        #[handler::manual]
        fn on_invoke(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
            invoke: #program::Invoke,
        ) {
            use ::aether_actor::OutboundReply;
            match self.programs.admit(&invoke) {
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
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
            invoked: #program::Invoked,
        ) {
            use ::aether_actor::OutboundReply;
            let Some((child, reply)) = self.programs.finish(&invoked, ctx.source_mailbox()) else {
                return;
            };
            if let Some(reply) = reply {
                ctx.reply_to(reply, &invoked);
            }
            ctx.despawn_inline_child(child);
        }
    }
}

fn expand_table(table: &Ident, programs: &[ProgramEntry], program: &TokenStream2) -> TokenStream2 {
    let entries = programs.iter().map(|entry| {
        let ty = &entry.ty;
        quote! { #program::ProgramEntry::of::<#ty>() }
    });
    quote! {
        static #table: #program::ProgramTable =
            #program::__macro_internals::program_table(&[#(#entries),*]);
    }
}

fn expand_invocation(root: &Ident, invocation: &Ident, table: &Ident, program: &TokenStream2) -> TokenStream2 {
    let namespace = format!("{BUNDLE_NAMESPACE}.invocation");
    quote! {
        struct #invocation;

        #[::aether_actor::actor(instanced, child_of(#root))]
        impl ::aether_actor::WasmActor for #invocation {
            const NAMESPACE: &'static str = #namespace;

            fn init(
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                Ok(Self)
            }

            #[handler::single]
            fn on_invoke(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_>,
                invoke: #program::Invoke,
            ) {
                let _ = self;
                let invoked = #program::dispatch(&#table, invoke);
                if let Some(parent) = ctx.source_mailbox() {
                    ctx.send_to(parent, &invoked);
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
    quote! {
        const #len_ident: usize = #program::__macro_internals::program_record_len(
            #name.as_bytes(),
            #intent.as_bytes(),
        );
        const #bytes_ident: [u8; #len_ident] = #program::__macro_internals::write_program_record::<#len_ident>(
            #name.as_bytes(),
            <#input as #program::__macro_internals::Kind>::ID.0,
            <#result as #program::__macro_internals::Kind>::ID.0,
            #program::__macro_internals::MODE_PURE,
            #intent.as_bytes(),
        );
        const _: &[u8] = &#bytes_ident;
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = #PROGRAMS_SECTION)]
        static #section_ident: [u8; #len_ident] = #bytes_ident;
    }
}
