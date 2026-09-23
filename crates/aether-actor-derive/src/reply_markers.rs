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

/// The type one handler's `Contract<K>` row names as its reply (ADR-0231 §1):
/// `O` for a single `-> O` or `-> Pending<O>` handler, `Silent` for `-> ()`,
/// and `Undeclared` for a manual handler, whose class decides regardless of
/// its return type.
pub fn contract_reply_ty(class: HandlerClass, reply: &HandlerReply) -> TokenStream2 {
    match (class, reply.manifest_kind()) {
        (HandlerClass::Manual, _) => quote! { ::aether_actor::Undeclared },
        (HandlerClass::Single, Some(reply_ty)) => quote! { #reply_ty },
        (HandlerClass::Single, None) => quote! { ::aether_actor::Silent },
    }
}

/// Emit one handler's `Contract<K>` row onto the site's impl header, gated by
/// the site's `#[cfg]`s.
pub fn contract_row_impl(
    class: HandlerClass,
    reply: &HandlerReply,
    kind_ty: &Type,
    site: &ReplyMarkerSite<'_>,
) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, self_ty, where_clause, cfgs } = site;
    let reply_ty = contract_reply_ty(class, reply);
    quote! {
        #(#cfgs)*
        impl #impl_generics ::aether_actor::Contract<#kind_ty> for #self_ty #where_clause {
            type Reply = #reply_ty;
        }
    }
}

/// One `CONTRACTS` element for a handler, carrying the handler's `#[cfg]`s.
///
/// The reply half reads `<R as ReplyShape>::CONTRACT` off the same type the
/// handler's `Contract<K>` row names, so the list is derived from the rows
/// and cannot disagree with them.
pub fn contract_element(class: HandlerClass, reply: &HandlerReply, kind_ty: &Type, cfgs: &[Attribute]) -> TokenStream2 {
    let reply_ty = contract_reply_ty(class, reply);
    quote! {
        #(#cfgs)*
        (
            <#kind_ty as ::aether_actor::__macro_internals::Kind>::ID,
            <#reply_ty as ::aether_actor::ReplyShape>::CONTRACT,
        )
    }
}

/// The `(KindId, ReplyContract)` element type of every `CONTRACTS` list.
pub fn contract_element_ty() -> TokenStream2 {
    quote! {
        (
            ::aether_actor::__macro_internals::KindId,
            ::aether_actor::__macro_internals::ReplyContract,
        )
    }
}

/// A `&'static [(KindId, ReplyContract)]` expression over `elements`, each of
/// which carries its own handler's `#[cfg]`s.
pub fn contract_rows_expr(elements: &[TokenStream2]) -> TokenStream2 {
    quote! { &[#(#elements),*] }
}

/// A const block expression concatenating `parts`, each a
/// `&'static [(KindId, ReplyContract)]` expression, into one slice of that type.
///
/// The consts in it are nested items, where `Self` does not resolve, so a part
/// that reads an associated const names its type concretely.
pub fn concat_contract_rows(parts: &[TokenStream2]) -> TokenStream2 {
    let element_ty = contract_element_ty();
    quote! {
        {
            const PARTS: &'static [&'static [#element_ty]] = &[#(#parts),*];
            const LEN: usize = {
                let mut len = 0;
                let mut index = 0;
                while index < PARTS.len() {
                    len += PARTS[index].len();
                    index += 1;
                }
                len
            };
            const ALL: [#element_ty; LEN] = {
                let mut out = [(
                    ::aether_actor::__macro_internals::KindId(0),
                    ::aether_actor::__macro_internals::ReplyContract::None,
                ); LEN];
                let mut pos = 0;
                let mut index = 0;
                while index < PARTS.len() {
                    let part = PARTS[index];
                    let mut row = 0;
                    while row < part.len() {
                        out[pos] = part[row];
                        pos += 1;
                        row += 1;
                    }
                    index += 1;
                }
                out
            };
            &ALL
        }
    }
}

/// Emit an actor's `Contracts` impl: its `local` rows (a slice expression),
/// followed by an adopted handler set's rows when `set` carries an expression
/// for them (ADR-0169). The set's rows are already `#[cfg]`-resolved in the
/// crate that defines the set (ADR-0183).
pub fn contracts_impl(site: &ReplyMarkerSite<'_>, local: &TokenStream2, set: Option<&TokenStream2>) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, self_ty, where_clause, .. } = site;
    let element_ty = contract_element_ty();
    let value = match set {
        None => quote! { #local },
        Some(set_rows) => concat_contract_rows(&[local.clone(), set_rows.clone()]),
    };
    quote! {
        impl #impl_generics ::aether_actor::Contracts for #self_ty #where_clause {
            const CONTRACTS: &'static [#element_ty] = #value;
        }
    }
}
