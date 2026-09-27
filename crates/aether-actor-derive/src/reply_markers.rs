use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
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

/// Emit the reply-contract marker that follows from one handler signature,
/// plus the requirement that the handler's kind crosses actors.
///
/// `HandlesKind<K>` is emitted separately at every call site. This helper keeps
/// the companion marker derived from the same parsed class / return shape on
/// the wasm, native identity, and handler-set paths, which is also why the
/// reach requirement lives here: every handler passes through it.
pub fn reply_marker_impl(
    class: HandlerClass,
    reply: &HandlerReply,
    kind_ty: &Type,
    site: &ReplyMarkerSite<'_>,
) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, self_ty, where_clause, cfgs } = site;
    let crosses = crosses_actors_requirement(kind_ty, site);
    let marker = match (class, reply) {
        (HandlerClass::Single, HandlerReply::Sync(reply_ty) | HandlerReply::Deferred(reply_ty)) => quote! {
            #(#cfgs)*
            impl #impl_generics ::aether_actor::Replies<#kind_ty> for #self_ty #where_clause {
                type Reply = #reply_ty;
            }
        },
        (HandlerClass::Single, HandlerReply::None) | (HandlerClass::Manual, _) => quote! {},
    };
    quote! { #crosses #marker }
}

/// Require a handler's kind to cross actors (ADR-0242), so no handler receives
/// a kind of actor reach, and one injected through a raw door finds no row.
///
/// The check is a generic function over the site's own generics, so a kind
/// written in terms of them resolves, and it carries the kind's span, so the
/// reach diagnostic points at the handler's kind parameter.
fn crosses_actors_requirement(kind_ty: &Type, site: &ReplyMarkerSite<'_>) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, where_clause, cfgs, .. } = site;
    let call = quote_spanned! {kind_ty.span()=>
        __aether_crosses::<#kind_ty>();
    };
    quote! {
        #(#cfgs)*
        const _: () = {
            #[allow(dead_code)]
            fn __aether_handler_kind_crosses_actors #impl_generics () #where_clause {
                fn __aether_crosses<K: ?::core::marker::Sized + ::aether_actor::__macro_internals::CrossesActors>() {}
                #call
            }
        };
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
/// the site's `#[cfg]`s, naming `index` as the row's position in the actor's
/// `Contracts::Rows` list (ADR-0231 §10).
pub fn contract_row_impl(
    class: HandlerClass,
    reply: &HandlerReply,
    kind_ty: &Type,
    index: &TokenStream2,
    site: &ReplyMarkerSite<'_>,
) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, self_ty, where_clause, cfgs } = site;
    let reply_ty = contract_reply_ty(class, reply);
    quote! {
        #(#cfgs)*
        impl #impl_generics ::aether_actor::Contract<#kind_ty> for #self_ty #where_clause {
            type Reply = #reply_ty;
            type Index = #index;
        }
    }
}

/// The position `steps` entries past `base` in a declaration list (ADR-0231
/// §10): `base` wrapped in one `There` per step.
pub fn position_past(base: TokenStream2, steps: usize) -> TokenStream2 {
    (0..steps).fold(base, |inner, _| quote! { ::aether_actor::There<#inner> })
}

/// The position of entry `index` in a declaration list: `Here` for the head,
/// `There<…>` past it.
pub fn position(index: usize) -> TokenStream2 {
    position_past(quote! { ::aether_actor::Here }, index)
}

/// A type-level declaration list `(E1, (E2, (…, tail)))` over `entries`.
pub fn declaration_list(entries: impl DoubleEndedIterator<Item = TokenStream2>, tail: TokenStream2) -> TokenStream2 {
    entries.rev().fold(tail, |rest, entry| quote! { (#entry, #rest) })
}

/// The `Row<K, O>` entry one handler contributes to its actor's
/// `Contracts::Rows` list: the kind and the reply its `Contract<K>` row names.
pub fn row_entry(class: HandlerClass, reply: &HandlerReply, kind_ty: &Type) -> TokenStream2 {
    let reply_ty = contract_reply_ty(class, reply);
    quote! { ::aether_actor::Row<#kind_ty, #reply_ty> }
}

/// One handler as its contract row list sees it: class, reply, kind, and the
/// `#[cfg]`s that decide whether its slot holds its row or `Gap`.
pub struct RowSpec<'a> {
    pub class: HandlerClass,
    pub reply: &'a HandlerReply,
    pub kind_ty: &'a Type,
    pub cfgs: &'a [Attribute],
}

/// An actor's contract row list (ADR-0231 §10): the `Contracts::Rows` type,
/// plus the type aliases that pick a gated handler's slot.
pub struct RowsList {
    /// One `#[cfg]`-ed alias pair per gated handler: its `Row<K, O>` when the
    /// handler's predicates hold and `Gap` when they do not. Emitted beside
    /// the `Contracts` impl that names them.
    pub aliases: TokenStream2,
    /// `(E1, (E2, (…, tail)))`, one entry per handler in declaration order.
    pub list: TokenStream2,
}

/// Build the contract row list over `rows`, ending in `tail`. A handler with
/// no `#[cfg]` contributes its `Row<K, O>` inline; a gated one contributes an
/// alias that resolves to its row or to `Gap` in this configuration, so it
/// keeps its slot and every later handler keeps its position.
pub fn rows_list(rows: &[RowSpec<'_>], tail: TokenStream2) -> syn::Result<RowsList> {
    let mut aliases = Vec::new();
    let mut entries = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let entry = row_entry(row.class, row.reply, row.kind_ty);
        if row.cfgs.is_empty() {
            entries.push(entry);
            continue;
        }

        let alias = format_ident!("__AetherRow{}", index);
        let predicate = conjoined_cfg_predicate(row.cfgs)?;
        aliases.push(quote! {
            #[cfg(#predicate)]
            type #alias = #entry;
            #[cfg(not(#predicate))]
            type #alias = ::aether_actor::Gap;
        });
        entries.push(quote! { #alias });
    }
    Ok(RowsList { aliases: quote! { #(#aliases)* }, list: declaration_list(entries.into_iter(), tail) })
}

/// The conjunction of a handler's `#[cfg]` predicates, as `all(P1, …, Pn)`.
///
/// Purely syntactic: the macro reads the predicate tokens the author wrote and
/// never evaluates them, so any predicate rustc accepts — including a custom
/// `--cfg` flag from a build script — rides through unexamined, and an
/// ill-formed one is diagnosed by rustc at the author's own span. Stacking the
/// attributes would express the conjunction on the positive arm, but the
/// negative arm needs the predicate as a term, so it is built once here.
pub fn conjoined_cfg_predicate(cfgs: &[Attribute]) -> syn::Result<TokenStream2> {
    let predicates =
        cfgs.iter().map(|attr| Ok(attr.meta.require_list()?.tokens.clone())).collect::<syn::Result<Vec<_>>>()?;
    Ok(quote! { all(#(#predicates),*) })
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

/// Emit an actor's `Contracts` impl: its `rows` list type (ADR-0231 §10) and
/// its `local` rows (a slice expression), followed by an adopted handler set's
/// rows when `set` carries an expression for them (ADR-0169). The set's rows
/// are already `#[cfg]`-resolved in the crate that defines the set
/// (ADR-0183).
///
/// `aliases` are the gated slots' alias pairs the `rows` type names. When
/// there are any, they and the impl share an anonymous `const _` block, so the
/// aliases need no name unique beyond this one actor.
pub fn contracts_impl(
    site: &ReplyMarkerSite<'_>,
    aliases: &TokenStream2,
    rows: &TokenStream2,
    local: &TokenStream2,
    set: Option<&TokenStream2>,
) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, self_ty, where_clause, .. } = site;
    let element_ty = contract_element_ty();
    let value = match set {
        None => quote! { #local },
        Some(set_rows) => concat_contract_rows(&[local.clone(), set_rows.clone()]),
    };
    let contracts = quote! {
        impl #impl_generics ::aether_actor::Contracts for #self_ty #where_clause {
            type Rows = #rows;
            const CONTRACTS: &'static [#element_ty] = #value;
        }
    };
    if aliases.is_empty() {
        return contracts;
    }

    quote! {
        const _: () = {
            #aliases
            #contracts
        };
    }
}

/// Emit an actor's one `Declared` impl (ADR-0231 §10): its `depends(..)` and
/// `spawns(..)` lists, each a type-level list in declaration order.
pub fn declared_impl(site: &ReplyMarkerSite<'_>, depends: &[syn::TypePath], spawns: &[syn::TypePath]) -> TokenStream2 {
    let ReplyMarkerSite { impl_generics, self_ty, where_clause, .. } = site;
    let depends_list = declaration_list(depends.iter().map(|target| quote! { #target }), quote! { () });
    let spawns_list = declaration_list(spawns.iter().map(|child| quote! { #child }), quote! { () });
    quote! {
        impl #impl_generics ::aether_actor::Declared for #self_ty #where_clause {
            type Depends = #depends_list;
            type Spawns = #spawns_list;
        }
    }
}
