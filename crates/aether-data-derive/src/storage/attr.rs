//! `#[storage(strict)]` and repeatable `#[storage(was = "…")]`.

use syn::{Attribute, Expr, Field, Lit, Meta};

pub(super) struct TypeStorageAttr {
    pub(super) strict: bool,
}

pub(super) struct FieldStorageAttr {
    pub(super) aliases: Vec<(String, proc_macro2::Span)>,
}

pub(super) fn parse_type_storage(attrs: &[Attribute]) -> syn::Result<TypeStorageAttr> {
    let mut strict = false;
    for attr in attrs {
        if !attr.path().is_ident("storage") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("strict") {
                if meta.input.peek(syn::Token![=]) {
                    return Err(meta.error("`strict` is a flag, not a key-value"));
                }
                strict = true;
                return Ok(());
            }
            if meta.path.is_ident("was") {
                return Err(meta.error("`was` is a field attribute, not a type attribute"));
            }
            Err(meta.error("expected `strict`"))
        })?;
    }
    Ok(TypeStorageAttr { strict })
}

pub(super) fn parse_field_storage(field: &Field) -> syn::Result<FieldStorageAttr> {
    parse_storage_aliases(&field.attrs)
}

pub(super) fn parse_storage_aliases(attrs: &[Attribute]) -> syn::Result<FieldStorageAttr> {
    let mut aliases = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("storage") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("was") {
                let value = meta.value()?;
                let expr: Expr = value.parse()?;
                let Expr::Lit(lit) = &expr else {
                    return Err(meta.error("`was` must be a string literal"));
                };
                let Lit::Str(s) = &lit.lit else {
                    return Err(meta.error("`was` must be a string literal"));
                };
                aliases.push((s.value(), s.span()));
                return Ok(());
            }
            if meta.path.is_ident("strict") {
                return Err(meta.error("`strict` is a type attribute, not a field attribute"));
            }
            Err(meta.error("expected `was = \"...\"`"))
        })?;
    }
    Ok(FieldStorageAttr { aliases })
}

pub(super) fn repr_c_attr(attrs: &[Attribute]) -> Option<&Attribute> {
    attrs.iter().find(|attr| {
        if !attr.path().is_ident("repr") {
            return false;
        }
        let Meta::List(list) = &attr.meta else {
            return false;
        };
        let mut has_c = false;
        let _ = list.parse_nested_meta(|meta| {
            if meta.path.is_ident("C") {
                has_c = true;
            }
            Ok(())
        });
        has_c
    })
}
