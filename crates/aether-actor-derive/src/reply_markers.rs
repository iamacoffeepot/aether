use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Type};

use crate::handler_parse::{HandlerClass, HandlerReply};

/// Emit the reply-contract marker that follows from one handler signature.
///
/// `HandlesKind<K>` is emitted separately at every call site. This helper keeps
/// the companion marker derived from the same parsed class / return shape on
/// the wasm, native identity, and handler-set paths.
#[allow(clippy::too_many_arguments)]
pub fn reply_marker_impl(
    class: HandlerClass,
    reply: &HandlerReply,
    kind_ty: &Type,
    multi_kind: Option<&Type>,
    impl_generics: &TokenStream2,
    self_ty: &TokenStream2,
    where_clause: &TokenStream2,
    cfgs: &[Attribute],
) -> TokenStream2 {
    match (class, reply) {
        (HandlerClass::Single, HandlerReply::Sync(reply_ty) | HandlerReply::Deferred(reply_ty)) => quote! {
            #(#cfgs)*
            impl #impl_generics ::aether_actor::Replies<#kind_ty> for #self_ty #where_clause {
                type Reply = #reply_ty;
            }
        },
        (HandlerClass::Multi, _) => {
            let item_ty = multi_kind.expect("multi_kind_or_return_error supplies every multi handler's emit kind");
            quote! {
                #(#cfgs)*
                impl #impl_generics ::aether_actor::Streams<#kind_ty> for #self_ty #where_clause {
                    type Item = #item_ty;
                }
            }
        }
        (HandlerClass::Single, HandlerReply::None) | (HandlerClass::Manual, _) => quote! {},
    }
}
