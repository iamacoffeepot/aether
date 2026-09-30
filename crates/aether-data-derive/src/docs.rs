//! `Schema::DOC_NODE` emission: the `///` docs of a type's fields and
//! variants as a `DocNode` literal mirroring its `SCHEMA`.
//!
//! An undocumented field or variant is emitted as `Doc::Missing` holding a
//! message that names it, because the const check that refuses it cannot
//! format one. Nothing refuses a missing doc here: only a type a program
//! input reaches must be documented, and that check runs at the program.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, DataEnum, Expr, ExprLit, Fields, Lit, Meta, Type};

use crate::{FieldInfo, is_vec_u8};

/// The joined text of `attrs`' `///` lines, each with one leading space
/// stripped, trimmed. `None` when there is none or it is blank.
pub fn doc_text(attrs: &[Attribute]) -> Option<String> {
    let lines: Vec<String> = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(pair) => match &pair.value {
                Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) => Some(text.value()),
                _ => None,
            },
            _ => None,
        })
        .map(|line| line.strip_prefix(' ').map(str::to_owned).unwrap_or(line))
        .collect();
    let text = lines.join("\n").trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The `DocNode` literal for a struct: `Leaf` for a fieldless struct, whose
/// schema is `Unit`.
pub fn doc_node_struct(type_ident: &str, fields: &[FieldInfo]) -> TokenStream2 {
    if fields.is_empty() {
        return quote! { ::aether_data::__derive_runtime::DocNode::Leaf };
    }
    let entries = fields.iter().enumerate().map(|(index, field)| {
        let path = match &field.ident {
            Some(ident) => format!("{type_ident}.{ident}"),
            None => format!("{type_ident}.{index}"),
        };
        field_doc(&path, doc_text(&field.attrs), &field.ty, "field")
    });
    quote! {
        ::aether_data::__derive_runtime::DocNode::Struct {
            fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[ #( #entries ),* ]),
        }
    }
}

/// The `DocNode` literal for an enum. A tuple variant's positional fields
/// carry an empty doc: they have no description of their own, but the check
/// still walks their types.
pub fn doc_node_enum(type_ident: &str, data: &DataEnum) -> TokenStream2 {
    let variants = data.variants.iter().map(|variant| {
        let path = format!("{type_ident}::{}", variant.ident);
        let doc = doc_expr(&path, doc_text(&variant.attrs), "variant");
        let fields: Vec<TokenStream2> = match &variant.fields {
            Fields::Unit => Vec::new(),
            Fields::Unnamed(unnamed) => unnamed
                .unnamed
                .iter()
                .enumerate()
                .map(|(index, field)| field_doc(&format!("{path}.{index}"), Some(String::new()), &field.ty, "field"))
                .collect(),
            Fields::Named(named) => named
                .named
                .iter()
                .map(|field| {
                    let name = field.ident.as_ref().map(ToString::to_string).unwrap_or_default();
                    field_doc(&format!("{path}.{name}"), doc_text(&field.attrs), &field.ty, "field")
                })
                .collect(),
        };
        quote! {
            ::aether_data::__derive_runtime::VariantDoc {
                doc: #doc,
                fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[ #( #fields ),* ]),
            }
        }
    });
    quote! {
        ::aether_data::__derive_runtime::DocNode::Enum {
            variants: ::aether_data::__derive_runtime::Cow::Borrowed(&[ #( #variants ),* ]),
        }
    }
}

fn field_doc(path: &str, doc: Option<String>, ty: &Type, what: &str) -> TokenStream2 {
    let doc = doc_expr(path, doc, what);
    let node = field_node_expr(ty);
    let opaque = format!(
        "`{path}` has a struct or enum type with no doc tree (a hand-written `Schema`); a program input cannot expose it"
    );
    quote! {
        ::aether_data::__derive_runtime::FieldDoc {
            doc: #doc,
            node: ::aether_data::__derive_runtime::DocCell::Static(&#node),
            opaque: #opaque,
        }
    }
}

fn doc_expr(path: &str, doc: Option<String>, what: &str) -> TokenStream2 {
    if let Some(text) = doc {
        quote! {
            ::aether_data::__derive_runtime::Doc::Written(::aether_data::__derive_runtime::Cow::Borrowed(#text))
        }
    } else {
        let message = format!("`{path}` has no `///` doc; every {what} a program input exposes needs one");
        quote! { ::aether_data::__derive_runtime::Doc::Missing(#message) }
    }
}

/// `<T as Schema>::DOC_NODE`, except a syntactic `Vec<u8>`, whose schema the
/// derive writes as `Bytes`: a leaf.
fn field_node_expr(ty: &Type) -> TokenStream2 {
    if is_vec_u8(ty) {
        quote! { ::aether_data::__derive_runtime::DocNode::Leaf }
    } else {
        quote! { <#ty as ::aether_data::Schema>::DOC_NODE }
    }
}

#[cfg(test)]
mod tests {
    use super::doc_text;
    use syn::parse_quote;

    #[test]
    fn doc_lines_join_with_one_leading_space_stripped() {
        // Catches a joiner that keeps the `///` space on every line or drops interior indentation.
        let item: syn::ItemStruct = parse_quote! {
            /// First line.
            ///   indented
            #[derive(Clone)]
            struct S;
        };
        assert_eq!(doc_text(&item.attrs).as_deref(), Some("First line.\n  indented"));
    }

    #[test]
    fn a_blank_doc_is_missing() {
        // Catches `///` with no text passing as documented.
        let item: syn::ItemStruct = parse_quote! {
            ///
            struct S;
        };
        assert_eq!(doc_text(&item.attrs), None);
    }
}
