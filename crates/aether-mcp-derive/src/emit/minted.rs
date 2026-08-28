//! The three sibling types minted per tool.
//!
//! All three are structural wrappers, the same technique `#[http::router]` uses
//! to give a route a kind of its own. Each earns its layer:
//!
//! - the **request** wrapper gives the tool a distinct `KindId`, which is the
//!   actor dispatcher's handler slot and the identifier the capability
//!   recomputes at registration;
//! - the **value** wrapper is what a provider actually serializes, and wrapping
//!   the output one level deeper keeps the boundary's "exactly one of these is
//!   non-null" invariant true even when `Output` is unit and serializes as
//!   null;
//! - the **boundary** wrapper is the advertised `outputSchema`, and it is
//!   object-shaped for every `Output`, so an addressed spill still conforms to
//!   the schema the tool advertised.
//!
//! None carries `repr(C)`, so each takes the structured wire path. That is
//! load-bearing rather than incidental: the schema codec selects a cast layout
//! only at its top-level entry, so a `repr(C)` `Input` or `Output` nested
//! inside these wrappers is still encoded structurally on both sides.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::model::Tool;

/// Emit the request, value, and boundary siblings for one tool.
pub fn siblings(tool: &Tool) -> TokenStream2 {
    let Tool { request_struct, value_struct, boundary_struct, kind_name, input, output, .. } = tool;
    quote! {
        #[doc(hidden)]
        #[derive(
            ::aether_data::Kind,
            ::aether_data::Schema,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[kind(name = #kind_name)]
        pub struct #request_struct {
            pub input: #input,
        }

        #[doc(hidden)]
        #[derive(::aether_data::Schema, ::serde::Serialize, ::serde::Deserialize)]
        struct #value_struct {
            output: #output,
        }

        #[doc(hidden)]
        #[derive(::aether_data::Schema, ::serde::Serialize, ::serde::Deserialize)]
        struct #boundary_struct {
            inline: ::core::option::Option<#value_struct>,
            addressed: ::core::option::Option<::aether_mcp::AddressedOutput>,
        }
    }
}
