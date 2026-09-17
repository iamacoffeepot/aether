//! Turn an author trigger pattern into a `matches!` test that binds nothing.

use syn::spanned::Spanned;
use syn::{Pat, parse_quote_spanned};

pub fn match_test(pat: &Pat) -> Pat {
    match pat {
        Pat::Ident(ident) => ident
            .subpat
            .as_ref()
            .map_or_else(|| parse_quote_spanned!(ident.ident.span() => _), |(_, sub)| match_test(sub)),
        Pat::Wild(_) | Pat::Lit(_) | Pat::Path(_) | Pat::Rest(_) | Pat::Range(_) | Pat::Const(_) => pat.clone(),
        Pat::Or(or) => {
            let mut or = or.clone();
            or.cases = or.cases.into_iter().map(|case| match_test(&case)).collect();
            Pat::Or(or)
        }
        Pat::Paren(paren) => {
            let mut paren = paren.clone();
            *paren.pat = match_test(&paren.pat);
            Pat::Paren(paren)
        }
        Pat::Reference(reference) => {
            let mut reference = reference.clone();
            *reference.pat = match_test(&reference.pat);
            Pat::Reference(reference)
        }
        Pat::Tuple(tuple) => {
            let mut tuple = tuple.clone();
            tuple.elems = tuple.elems.into_iter().map(|elem| match_test(&elem)).collect();
            Pat::Tuple(tuple)
        }
        Pat::TupleStruct(tuple) => {
            let mut tuple = tuple.clone();
            tuple.elems = tuple.elems.into_iter().map(|elem| match_test(&elem)).collect();
            Pat::TupleStruct(tuple)
        }
        Pat::Struct(structure) => {
            let mut structure = structure.clone();
            for field in &mut structure.fields {
                let span = field.pat.span();
                field.pat = Box::new(match_test(&field.pat));
                if field.colon_token.is_none() {
                    field.colon_token = Some(syn::Token![:](span));
                }
            }
            Pat::Struct(structure)
        }
        Pat::Slice(slice) => {
            let mut slice = slice.clone();
            slice.elems = slice.elems.into_iter().map(|elem| match_test(&elem)).collect();
            Pat::Slice(slice)
        }
        Pat::Type(typed) => match_test(&typed.pat),
        Pat::Macro(_) | Pat::Verbatim(_) => pat.clone(),
        _ => parse_quote_spanned!(pat.span() => _),
    }
}
