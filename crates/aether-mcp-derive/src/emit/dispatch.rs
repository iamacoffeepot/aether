//! One `#[handler::manual]` dispatcher per tool.
//!
//! `manual` rather than `single` because a deferring tool must be able to hold
//! its reply obligation: the dispatcher answers only when the method decided
//! now, and stays silent when `defer` has already forwarded the obligation to a
//! peer.

use quote::quote;
use syn::{ImplItemFn, parse_quote};

use crate::emit::answer_from;
use crate::model::Tool;

/// The dispatcher for one tool: unwrap the minted request, build the tool
/// context, call the retained method, and answer unless it deferred.
pub fn handler(tool: &Tool) -> ImplItemFn {
    let Tool { dispatch_name, request_struct, value_struct, ctx, host, method, docs, .. } = tool;

    let state = host.parameter();
    let invoke = host.call(method, &quote!(__aether_context, __aether_mail.input));
    // The tool context borrows the transport for the duration of the call, and
    // the answering arms need it back, so the borrow is confined to a block
    // that ends with the outcome.
    let outcome = quote! {
        let __aether_outcome = {
            let __aether_context = ::aether_mcp::Context::new(
                &mut *__aether_ctx,
                <#request_struct as ::aether_data::Kind>::ID,
            );
            ::aether_mcp::Outcome::from(#invoke)
        };
    };
    let answer = answer_from(value_struct, &quote!(__aether_answer));

    parse_quote! {
        #(#docs)*
        #[handler::manual]
        fn #dispatch_name(#state, __aether_ctx: &mut #ctx, __aether_mail: #request_struct) {
            #outcome
            match __aether_outcome {
                ::aether_mcp::Outcome::Reply(__aether_answer) => {
                    let __aether_result = #answer;
                    __aether_ctx.take_inbound().reply(&__aether_result);
                }
                // `defer` already forwarded the inherited send with the tool's
                // source stashed under its correlation, so the call is still
                // open and a reply mapping answers it later.
                ::aether_mcp::Outcome::Deferred => {}
            }
        }
    }
}
