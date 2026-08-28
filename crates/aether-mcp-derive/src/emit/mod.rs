//! Code generation for one parsed [`Router`].
//!
//! The emitters assume a settled parse: every name is minted, every mapping is
//! grouped, and every signature has been checked. Nothing here returns a
//! `syn::Result`, because a diagnostic raised this late would point at
//! generated tokens rather than at the author's.

pub mod dispatch;
pub mod minted;
pub mod registration;
pub mod reply;

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Ident, ImplItem, ImplItemFn, ItemImpl, parse_quote};

use crate::model::Fallback;
use crate::parse::Router;

/// Expand `router` over `item`, returning the minted siblings followed by the
/// rewritten impl block.
pub fn router(mut item: ItemImpl, router: &Router) -> TokenStream2 {
    let siblings: Vec<TokenStream2> = router.tools.iter().map(minted::siblings).collect();

    let mut handlers: Vec<ImplItemFn> = router.tools.iter().map(dispatch::handler).collect();
    for group in &router.groups {
        match &group.fallback {
            // A retained manual handler keeps its own slot: the branches are
            // injected into it, so this group emits no handler of its own and
            // the one-handler-per-kind invariant survives.
            Fallback::Manual { method, ctx_ident, reply_ident } => {
                let branches = reply::probe(group, &router.tools, ctx_ident, reply_ident);
                inject_into(&mut item, method, &branches);
            }
            Fallback::Vacant { .. } => handlers.push(reply::vacant_handler(group, &router.tools)),
            Fallback::Http { .. } => handlers.push(reply::http_handler(group, &router.tools)),
        }
    }

    for handler in handlers {
        item.items.push(ImplItem::Fn(handler));
    }
    registration::inject(&mut item, router);

    quote! {
        #(#siblings)*
        #item
    }
}

/// Prepend `branches` to the retained handler's body.
///
/// The tool branches run before whatever the author wrote, and each returns
/// after answering, so the authored body still sees exactly the replies no tool
/// claimed. Injecting at the front rather than wrapping the body is what lets
/// the branches borrow the reply and leave the owned value standing.
fn inject_into(item: &mut ItemImpl, owner: &Ident, branches: &TokenStream2) {
    let retained = item.items.iter_mut().find_map(|entry| match entry {
        ImplItem::Fn(method) if method.sig.ident == *owner => Some(method),
        _ => None,
    });
    if let Some(method) = retained {
        method.block.stmts.insert(0, as_statement(branches));
    }
}

/// Turn a `Result<Output, ToolError>` expression into the
/// `ToolInvocationResult` a provider replies with.
///
/// Shared by the synchronous dispatcher and every reply branch: both reach the
/// same two-armed decision, and stating it once is what keeps the wrapper
/// construction — the step that binds the mapper's output to the tool's
/// declared type — in exactly one place.
pub fn answer_from(value_struct: &Ident, mapped: &TokenStream2) -> TokenStream2 {
    quote! {
        match #mapped {
            ::core::result::Result::Ok(__aether_output) => {
                let __aether_value = #value_struct { output: __aether_output };
                ::aether_mcp::ToolInvocationResult::Ok {
                    output_bytes: ::aether_data::wire::to_vec(&__aether_value).expect(
                        "the generated tool output wrapper serializes: its schema is derived from the \
                         declared Output, so a failure here is an internal invariant violation",
                    ),
                }
            }
            ::core::result::Result::Err(__aether_error) => {
                ::aether_mcp::ToolInvocationResult::from(__aether_error)
            }
        }
    }
}

/// A `wire`-shaped context type: the reply-class marker a handler carries is
/// dropped, because `wire` is a lifecycle hook with the default class.
/// `NativeCtx<'a, Manual>` becomes `NativeCtx<'a>`.
pub fn lifecycle_ctx(ty: &syn::Type) -> syn::Type {
    let mut base = ty.clone();
    if let syn::Type::Path(syn::TypePath { path, .. }) = &mut base
        && let Some(last) = path.segments.last_mut()
        && let syn::PathArguments::AngleBracketed(bracketed) = &mut last.arguments
    {
        bracketed.args =
            bracketed.args.iter().filter(|arg| matches!(arg, syn::GenericArgument::Lifetime(_))).cloned().collect();
        if bracketed.args.is_empty() {
            last.arguments = syn::PathArguments::None;
        }
    }
    base
}

/// The statement form of an emitted block, for pushing into an existing body.
pub fn as_statement(tokens: &TokenStream2) -> syn::Stmt {
    parse_quote!(#tokens)
}
