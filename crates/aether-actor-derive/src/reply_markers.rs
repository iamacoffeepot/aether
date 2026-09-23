use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Type};

use crate::handler_parse::{HandlerClass, HandlerReply};

/// The `impl` header a reply marker is pasted onto: generics, self type,
/// where-clause, and the handler's `#[cfg]`s.
pub struct ReplyMarkerSite<'a> {
    pub impl_generics: &'a TokenStream2,
    pub self_ty: &'a TokenStream2,
    pub where_clause: &'a TokenStream2,
    pub cfgs: &'a [Attribute],
}

/// Emit the reply-contract marker that follows from one handler signature.
///
/// `HandlesKind<K>` is emitted separately at every call site. This helper keeps
/// the companion marker derived from the same parsed class / return shape on
/// the wasm, native identity, and handler-set paths.
pub fn reply_marker_impl(
    class: HandlerClass,
    reply: &HandlerReply,
    kind_ty: &Type,
    site: &ReplyMarkerSite<'_>,
) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, self_ty, where_clause, cfgs } = site;
    match (class, reply) {
        (HandlerClass::Single, HandlerReply::Sync(reply_ty) | HandlerReply::Deferred(reply_ty)) => quote! {
            #(#cfgs)*
            impl #impl_generics ::aether_actor::Replies<#kind_ty> for #self_ty #where_clause {
                type Reply = #reply_ty;
            }
        },
        (HandlerClass::Single, HandlerReply::None) | (HandlerClass::Manual, _) => quote! {},
    }
}

/// The `::aether_data::ReplyContract` expression one native handler reports
/// in its `HandlerEntry` inventory row and its `HandlerCapability` row
/// (ADR-0231 §4). The class decides `Manual` from the attribute, never from the
/// return type; a single handler reads `One(R::ID)` for `-> R` /
/// `-> Pending<R>` and `None` for `-> ()`. All four native emitters read this
/// one mapping, so the manifest and the capability rows cannot drift apart.
pub fn native_reply_contract(class: HandlerClass, reply: &HandlerReply) -> TokenStream2 {
    match (class, reply.manifest_kind()) {
        (HandlerClass::Manual, _) => quote! { ::aether_data::ReplyContract::Manual },
        (HandlerClass::Single, Some(reply_ty)) => {
            quote! { ::aether_data::ReplyContract::One(<#reply_ty as ::aether_data::Kind>::ID) }
        }
        (HandlerClass::Single, None) => quote! { ::aether_data::ReplyContract::None },
    }
}
