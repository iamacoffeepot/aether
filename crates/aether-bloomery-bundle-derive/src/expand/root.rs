//! The one generated bundle root: one field, init, and handlers per present role.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::Ident;

pub struct RolePieces {
    pub field_name: Ident,
    pub field: TokenStream2,
    pub init: TokenStream2,
    pub handlers: TokenStream2,
    pub items: TokenStream2,
    /// The inline children this role's handlers spawn through the typed verbs,
    /// declared on the root's `#[actor(spawns(..))]` (ADR-0114).
    pub spawns: Vec<Ident>,
}

pub fn expand_root(root: &Ident, pieces: &[RolePieces]) -> TokenStream2 {
    let fields = pieces.iter().map(|piece| &piece.field);
    let inits = pieces.iter().map(|piece| &piece.init);
    let names = pieces.iter().map(|piece| &piece.field_name);
    let handlers = pieces.iter().map(|piece| &piece.handlers);
    let items = pieces.iter().map(|piece| &piece.items);
    let spawns: Vec<&Ident> = pieces.iter().flat_map(|piece| &piece.spawns).collect();
    let actor = if spawns.is_empty() {
        quote! { #[::aether_actor::actor] }
    } else {
        quote! { #[::aether_actor::actor(spawns(#(#spawns),*))] }
    };
    quote! {
        struct #root {
            #(#fields)*
        }

        #actor
        impl ::aether_actor::WasmActor for #root {
            const NAMESPACE: &'static str = ::aether_bloomery_bundle::BUNDLE_NAMESPACE;

            fn init(
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                #(#inits)*
                Ok(Self { #(#names),* })
            }

            #(#handlers)*
        }

        #(#items)*
    }
}
