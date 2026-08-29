//! `RegisterToolSelf` injection into `wire`.
//!
//! The claim is reflexive — it carries no mailbox field, because the capability
//! resolves the registrant from the inbound envelope's host-stamped `Source`.
//! That is what makes a registration unforgeable and gates it to in-process
//! actors, and it is why the send has to originate from the provider's own
//! `wire` rather than from anywhere that could name a different actor.
//!
//! `RegisterToolResult` is deliberately left unhandled, matching the HTTP
//! macro: an author who wants registration diagnostics claims that handler slot
//! themselves rather than having the macro take it.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, LitStr, Pat, PatType, parse_quote};

use crate::emit::lifecycle_ctx;
use crate::model::Tool;
use crate::parse::Router;

/// Append one registration per tool to `wire`, synthesizing the hook when the
/// impl has none.
pub fn inject(item: &mut ItemImpl, router: &Router) {
    let existing = item.items.iter_mut().find_map(|entry| match entry {
        ImplItem::Fn(method) if method.sig.ident == "wire" => Some(method),
        _ => None,
    });

    if let Some(wire) = existing {
        // An unnamed ctx parameter means the author's `wire` cannot be
        // addressed. That is a signature this macro does not get to reject —
        // it is a lifecycle hook, not a marked method — so the registration is
        // skipped and the missing tool surfaces as an unregistered name rather
        // than as a confusing error inside someone else's hook.
        if let Some(ctx) = ctx_binding(wire) {
            for tool in &router.tools {
                let send = registration(tool, &ctx, router.shared);
                wire.block.stmts.push(parse_quote!(#send));
            }
        }
        return;
    }

    let template = &router.tools[0];
    let ctx = format_ident!("__aether_ctx");
    let ctx_ty = lifecycle_ctx(&template.ctx);
    let state = template.host.parameter_for_lifecycle();
    let sends: Vec<TokenStream2> = router.tools.iter().map(|tool| registration(tool, &ctx, router.shared)).collect();
    let wire: ImplItemFn = parse_quote! {
        fn wire(#state, #ctx: &mut #ctx_ty) {
            #(#sends)*
        }
    };
    item.items.push(ImplItem::Fn(wire));
}

/// The ctx parameter's name in an author-written `wire`.
fn ctx_binding(wire: &ImplItemFn) -> Option<Ident> {
    match wire.sig.inputs.iter().nth(1) {
        Some(FnArg::Typed(PatType { pat, .. })) => match pat.as_ref() {
            Pat::Ident(named) => Some(named.ident.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// One tool's `RegisterToolSelf` send.
fn registration(tool: &Tool, ctx: &Ident, shared: bool) -> TokenStream2 {
    let Tool { metadata, kind_name, request_struct, value_struct, boundary_struct, .. } = tool;
    let name = LitStr::new(&metadata.name, metadata.name_span);
    let description = &metadata.description;
    let title = metadata.title.as_ref().map_or_else(
        || quote!(::core::option::Option::None),
        |text| quote!(::core::option::Option::Some(#text.to_owned())),
    );
    let (read_only, destructive) = (metadata.hints.read_only, metadata.hints.destructive);
    let (idempotent, open_world) = (metadata.hints.idempotent, metadata.hints.open_world);

    let request_bytes = carrier(request_struct);
    let value_bytes = carrier(value_struct);
    let boundary_bytes = carrier(boundary_struct);

    quote! {
        #ctx.actor::<::aether_mcp::McpServerCapability>()
            .send(&::aether_mcp::RegisterToolSelf {
                name: #name.to_owned(),
                title: #title,
                description: #description.to_owned(),
                annotations: ::aether_mcp::ToolAnnotations {
                    read_only: #read_only,
                    destructive: #destructive,
                    idempotent: #idempotent,
                    open_world: #open_world,
                },
                request_kind_name: #kind_name.to_owned(),
                request_kind: <#request_struct as ::aether_data::Kind>::ID,
                request_wrapper_schema_bytes: #request_bytes,
                output_wrapper_schema_bytes: #value_bytes,
                output_schema_bytes: #boundary_bytes,
                shared: #shared,
            });
    }
}

/// Canonical wire bytes of one generated schema.
///
/// A statically derived `SchemaType` its own serializer cannot encode is an
/// internal invariant violation, not a runtime condition, so the failure is a
/// generated expectation rather than a silent empty carrier the capability
/// would then reject with a confusing message.
fn carrier(schema_of: &Ident) -> TokenStream2 {
    quote! {
        ::aether_data::wire::to_vec(&<#schema_of as ::aether_data::Schema>::SCHEMA)
            .expect("a derived SchemaType serializes through the wire module it was derived against")
    }
}
