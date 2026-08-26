//! Derive-time refusals for ADR-0059 author mistakes.

use syn::{Data, DeriveInput, Fields};

use super::attr::{FieldStorageAttr, repr_c_attr};
use crate::KindAttr;

pub(super) fn check(input: &DeriveInput, kind: &KindAttr) -> syn::Result<()> {
    if let Data::Union(u) = &input.data {
        return Err(syn::Error::new_spanned(u.union_token, "Storage derive does not support unions"));
    }
    if let Some(attr) = repr_c_attr(&input.attrs) {
        return Err(syn::Error::new_spanned(
            attr,
            "Storage kinds cannot be `#[repr(C)]`; they are TLV-only (ADR-0059)",
        ));
    }
    refuse_reserved(kind.name.as_str(), "kind name", input.ident.span())?;
    match &input.data {
        Data::Struct(s) => check_fields(&s.fields)?,
        Data::Enum(e) => {
            for variant in &e.variants {
                refuse_reserved(&variant.ident.to_string(), "variant name", variant.ident.span())?;
                check_fields(&variant.fields)?;
            }
        }
        Data::Union(_) => {}
    }
    Ok(())
}

fn check_fields(fields: &Fields) -> syn::Result<()> {
    match fields {
        Fields::Named(named) => {
            for field in &named.named {
                let ident =
                    field.ident.as_ref().ok_or_else(|| syn::Error::new_spanned(field, "expected named field"))?;
                refuse_reserved(&ident.to_string(), "field name", ident.span())?;
                check_alias_names(&super::attr::parse_field_storage(field)?)?;
            }
        }
        Fields::Unnamed(unnamed) => {
            for field in &unnamed.unnamed {
                check_alias_names(&super::attr::parse_field_storage(field)?)?;
            }
        }
        Fields::Unit => {}
    }
    Ok(())
}

fn check_alias_names(attr: &FieldStorageAttr) -> syn::Result<()> {
    for (alias, span) in &attr.aliases {
        refuse_reserved(alias, "read alias", *span)?;
    }
    Ok(())
}

fn refuse_reserved(name: &str, what: &str, span: proc_macro2::Span) -> syn::Result<()> {
    if name.starts_with("__") {
        return Err(syn::Error::new(
            span,
            format!(
                "the `__` prefix is reserved for system-synthesized storage identifiers; {what} `{name}` is not allowed (ADR-0059 rule 4)"
            ),
        ));
    }
    Ok(())
}
