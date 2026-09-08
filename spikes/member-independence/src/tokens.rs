//! Token-stream hashing with identifiers normalized, and signature/body splits.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::{Fields, ImplItemFn, ItemConst, ItemFn, ItemImpl, ItemStatic, ItemTrait, ItemType, TraitItemFn};

pub fn hash_tokens(ts: &TokenStream) -> (String, usize) {
    let mut buf = String::new();
    let count = write_normalized(&mut buf, ts);
    (blake3::hash(buf.as_bytes()).to_hex().to_string(), count)
}

fn write_normalized(buf: &mut String, ts: &TokenStream) -> usize {
    let mut n = 0;
    for tt in ts.clone() {
        n += 1;
        match tt {
            TokenTree::Ident(_) => buf.push_str("I "),
            TokenTree::Punct(p) => {
                buf.push(p.as_char());
                buf.push(' ');
            }
            TokenTree::Literal(l) => {
                buf.push_str(&l.to_string());
                buf.push(' ');
            }
            TokenTree::Group(g) => {
                buf.push(open_delim(g.delimiter()));
                n += write_normalized(buf, &g.stream());
                buf.push(close_delim(g.delimiter()));
                buf.push(' ');
            }
        }
    }
    n
}

fn open_delim(d: Delimiter) -> char {
    match d {
        Delimiter::Parenthesis => '(',
        Delimiter::Brace => '{',
        Delimiter::Bracket => '[',
        Delimiter::None => '·',
    }
}

fn close_delim(d: Delimiter) -> char {
    match d {
        Delimiter::Parenthesis => ')',
        Delimiter::Brace => '}',
        Delimiter::Bracket => ']',
        Delimiter::None => '·',
    }
}

pub fn fn_parts(item: &ItemFn) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.vis.to_tokens(&mut sig);
    item.sig.to_tokens(&mut sig);
    (sig, item.block.to_token_stream())
}

pub fn impl_fn_parts(item: &ImplItemFn) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.vis.to_tokens(&mut sig);
    item.defaultness.to_tokens(&mut sig);
    item.sig.to_tokens(&mut sig);
    (sig, item.block.to_token_stream())
}

pub fn trait_fn_parts(item: &TraitItemFn) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.sig.to_tokens(&mut sig);
    let body = item.default.as_ref().map(ToTokens::to_token_stream).unwrap_or_default();
    (sig, body)
}

pub fn impl_parts(item: &ItemImpl) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.defaultness.to_tokens(&mut sig);
    item.unsafety.to_tokens(&mut sig);
    item.impl_token.to_tokens(&mut sig);
    item.generics.to_tokens(&mut sig);
    if let Some((bang, path, for_token)) = &item.trait_ {
        bang.to_tokens(&mut sig);
        path.to_tokens(&mut sig);
        for_token.to_tokens(&mut sig);
    }
    item.self_ty.to_tokens(&mut sig);
    item.generics.where_clause.to_tokens(&mut sig);
    let mut body = TokenStream::new();
    for it in &item.items {
        it.to_tokens(&mut body);
    }
    (sig, body)
}

pub fn trait_parts(item: &ItemTrait) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.vis.to_tokens(&mut sig);
    item.unsafety.to_tokens(&mut sig);
    item.auto_token.to_tokens(&mut sig);
    item.trait_token.to_tokens(&mut sig);
    item.ident.to_tokens(&mut sig);
    item.generics.to_tokens(&mut sig);
    item.colon_token.to_tokens(&mut sig);
    item.supertraits.to_tokens(&mut sig);
    item.generics.where_clause.to_tokens(&mut sig);
    let mut body = TokenStream::new();
    for it in &item.items {
        it.to_tokens(&mut body);
    }
    (sig, body)
}

pub fn fields_tokens(fields: &Fields) -> TokenStream {
    fields.to_token_stream()
}

pub fn const_parts(item: &ItemConst) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.vis.to_tokens(&mut sig);
    item.const_token.to_tokens(&mut sig);
    item.ident.to_tokens(&mut sig);
    item.generics.to_tokens(&mut sig);
    item.colon_token.to_tokens(&mut sig);
    item.ty.to_tokens(&mut sig);
    (sig, item.expr.to_token_stream())
}

pub fn static_parts(item: &ItemStatic) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.vis.to_tokens(&mut sig);
    item.static_token.to_tokens(&mut sig);
    item.mutability.to_tokens(&mut sig);
    item.ident.to_tokens(&mut sig);
    item.colon_token.to_tokens(&mut sig);
    item.ty.to_tokens(&mut sig);
    (sig, item.expr.to_token_stream())
}

pub fn type_parts(item: &ItemType) -> (TokenStream, TokenStream) {
    let mut sig = TokenStream::new();
    for a in &item.attrs {
        a.to_tokens(&mut sig);
    }
    item.vis.to_tokens(&mut sig);
    item.type_token.to_tokens(&mut sig);
    item.ident.to_tokens(&mut sig);
    item.generics.to_tokens(&mut sig);
    (sig, item.ty.to_token_stream())
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn renamed_idents_share_body_hash() {
        let a: ItemFn = parse_quote! {
            fn foo(x: i32) -> i32 { x + 1 }
        };
        let b: ItemFn = parse_quote! {
            fn bar(y: i32) -> i32 { y + 1 }
        };
        let (_, body_a) = fn_parts(&a);
        let (_, body_b) = fn_parts(&b);
        assert_eq!(hash_tokens(&body_a).0, hash_tokens(&body_b).0);
    }

    #[test]
    fn body_edit_changes_hash_not_signature() {
        let a: ItemFn = parse_quote! {
            pub fn hold(x: i32) -> i32 { x + 1 }
        };
        let b: ItemFn = parse_quote! {
            pub fn hold(x: i32) -> i32 { x + 2 }
        };
        let (sig_a, body_a) = fn_parts(&a);
        let (sig_b, body_b) = fn_parts(&b);
        assert_eq!(hash_tokens(&sig_a).0, hash_tokens(&sig_b).0);
        assert_ne!(hash_tokens(&body_a).0, hash_tokens(&body_b).0);
    }
}
