//! The composite reply handler — one per reply kind, however many tools,
//! routes, and authored correlations that kind serves.
//!
//! The whole surface turns on two facts the actor dispatcher enforces. A kind
//! has exactly one handler, and a reply carries exactly one stored request
//! context. So the branches here must *probe* for the tool context rather than
//! take it: taking it would consume an HTTP `DeferredSource` sitting under the
//! same correlation and leave `answer_deferred` with nothing to answer. Actor
//! dispatch is serialized, so nothing can consume the entry between the probe
//! and the take.
//!
//! Selection is by `tool_request_kind`, never by the reply kind. Three tools
//! can share one downstream reply, and the reply alone cannot say which one is
//! waiting.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Ident, ImplItemFn, parse_quote};

use crate::emit::answer_from;
use crate::model::{Fallback, Mapper, ReplyGroup, Tool};

/// The tool-context branches, addressed to the context and reply bindings the
/// enclosing handler names.
///
/// Emitted as one `if` statement so it can be either the whole body of a
/// generated handler or the first statement of a retained one.
pub fn probe(group: &ReplyGroup, tools: &[Tool], ctx: &Ident, reply: &Ident) -> TokenStream2 {
    let selection = select(group, tools, ctx, reply);
    quote! {
        if #ctx.in_reply_context_kind()
            == ::core::option::Option::Some(
                <::aether_mcp::DeferredToolSource as ::aether_data::Kind>::ID,
            )
            && let ::core::option::Option::Some(__aether_deferred) =
                #ctx.take_context::<::aether_mcp::DeferredToolSource>()
        {
            let __aether_result = #selection;
            ::aether_actor::OutboundReply::reply_to(#ctx, __aether_deferred.source, &__aether_result);
            return;
        }
    }
}

/// The `tool_request_kind` chain: one arm per mapping, then the refusal.
fn select(group: &ReplyGroup, tools: &[Tool], ctx: &Ident, reply: &Ident) -> TokenStream2 {
    let arms = group.mappings.iter().filter_map(|mapping| {
        let tool = tools.iter().find(|candidate| candidate.method == mapping.tool)?;
        let request = &tool.request_struct;
        let output = &tool.output;
        let call = match &mapping.mapper {
            // A branch mapper borrows, so the owned reply survives for the
            // fallback that follows.
            Mapper::Branch { method } => quote!(Self::#method(&#reply)),
            // A standalone mapping owns the reply. Only one arm ever runs and
            // it returns immediately, so the move is confined to that path.
            Mapper::Standalone { method, host } => host.call(method, &quote!(#ctx, #reply)),
        };
        // Binding the mapper's result at the tool's declared `Output` is what
        // holds a mapping and its tool to one type: a mismatch reports both
        // types here rather than surfacing as malformed bytes at the boundary.
        let mapped = quote! {{
            let __aether_mapped: ::core::result::Result<#output, ::aether_mcp::ToolError> = #call;
            __aether_mapped
        }};
        let answer = answer_from(&tool.value_struct, &mapped);
        Some(quote! {
            if __aether_deferred.tool_request_kind == <#request as ::aether_data::Kind>::ID {
                #answer
            } else
        })
    });

    quote! {
        #(#arms)* {
            // A stored tool context whose kind this actor does not map is a
            // real failure of the waiting call, so it is answered rather than
            // dropped into the fallback, which serves a different protocol.
            ::aether_mcp::ToolInvocationResult::Err {
                category: "unknown_tool".to_owned(),
                message: "this actor declares no mapping for the tool that opened this deferred call".to_owned(),
            }
        }
    }
}

/// A group made only of standalone mappings: the handler is the branches.
pub fn vacant_handler(group: &ReplyGroup, tools: &[Tool]) -> ImplItemFn {
    let Fallback::Vacant { host, ctx } = &group.fallback else {
        unreachable_fallback()
    };
    compose(group, tools, &host.parameter(), ctx, &quote!())
}

/// A group composed onto an `#[http::reply]` mapper: the retained mapper
/// answers whatever no tool claimed, through the HTTP deferral path.
pub fn http_handler(group: &ReplyGroup, tools: &[Tool]) -> ImplItemFn {
    let Fallback::Http { method, host, ctx } = &group.fallback else {
        unreachable_fallback()
    };
    let call = host.call(method, &quote!(__aether_ctx, __aether_reply));
    let tail = quote! {
        let __aether_response = #call;
        ::aether_http::answer_deferred(__aether_ctx, &__aether_response);
    };
    compose(group, tools, &host.parameter(), ctx, &tail)
}

fn compose(
    group: &ReplyGroup,
    tools: &[Tool],
    state: &TokenStream2,
    ctx: &syn::Type,
    tail: &TokenStream2,
) -> ImplItemFn {
    let name = &group.handler_name;
    let kind = &group.kind;
    let branches =
        probe(group, tools, &Ident::new("__aether_ctx", name.span()), &Ident::new("__aether_reply", name.span()));
    parse_quote! {
        #[handler::manual]
        fn #name(#state, __aether_ctx: &mut #ctx, __aether_reply: #kind) {
            #branches
            #tail
        }
    }
}

/// The emitter only routes a group to the handler builder its own fallback
/// selected, so the other arms cannot be reached.
fn unreachable_fallback() -> ! {
    panic!("a reply group was emitted through the wrong handler builder")
}
