//! The one generated bundle root: one field, init, and handlers per present role.
//!
//! Every bundle is content-addressed (ADR-0241 §3): the root emits the
//! [`CONTENT_ADDRESSED_SECTION`] marker, so the engine publishes it as
//! `aether.bloomery.bundle.<module hash>` and every built bundle is its own
//! publication. The root is `instanced`: the driver loads one per unit and
//! digest, each keyed by its unit, so it is born at
//! `aether.bloomery.bundle.<module hash>:<unit key>` (ADR-0241 §5).

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::Ident;

use aether_data::CONTENT_ADDRESSED_SECTION;

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
        quote! { #[::aether_actor::actor(instanced, root)] }
    } else {
        quote! { #[::aether_actor::actor(instanced, root, spawns(#(#spawns),*))] }
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

        // ADR-0241 §3: one version byte whose presence marks the module
        // content-addressed.
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = #CONTENT_ADDRESSED_SECTION)]
        static __AETHER_BLOOMERY_BUNDLE_CONTENT_ADDRESSED: [u8; 1] = [1u8];

        #(#items)*
    }
}
