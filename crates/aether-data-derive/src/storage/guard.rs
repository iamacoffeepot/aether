//! Derive-time refusals for ADR-0059 author mistakes.

use syn::{Data, DeriveInput, Field, Fields};

use super::attr::{FieldStorageAttr, repr_c_attr};
use crate::KindAttr;

pub(super) fn check(input: &DeriveInput, kind: Option<&KindAttr>) -> syn::Result<()> {
    if let Data::Union(u) = &input.data {
        return Err(syn::Error::new_spanned(u.union_token, "Storage derive does not support unions"));
    }
    if let Some(attr) = repr_c_attr(&input.attrs) {
        return Err(syn::Error::new_spanned(
            attr,
            "Storage kinds cannot be `#[repr(C)]`; they are TLV-only (ADR-0059)",
        ));
    }
    if let Some(kind) = kind {
        refuse_reserved(kind.name.as_str(), "kind name", input.ident.span())?;
    }
    let nested = kind.is_none();
    match &input.data {
        Data::Struct(s) => check_fields(&s.fields, nested)?,
        Data::Enum(e) => {
            for variant in &e.variants {
                refuse_reserved(&variant.ident.to_string(), "variant name", variant.ident.span())?;
                check_fields(&variant.fields, nested)?;
            }
        }
        Data::Union(_) => {}
    }
    Ok(())
}

pub(super) fn check_validate(input: &DeriveInput) -> syn::Result<()> {
    let ok = match &input.data {
        Data::Struct(s) => matches!(&s.fields, Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1),
        _ => false,
    };
    if ok {
        return Ok(());
    }
    Err(syn::Error::new(
        super::attr::flag_span(&input.attrs, "validate").unwrap_or_else(|| input.ident.span()),
        "`validate` applies to a tuple struct with exactly one field",
    ))
}

fn check_fields(fields: &Fields, nested: bool) -> syn::Result<()> {
    match fields {
        Fields::Named(named) => {
            for field in &named.named {
                let ident =
                    field.ident.as_ref().ok_or_else(|| syn::Error::new_spanned(field, "expected named field"))?;
                refuse_reserved(&ident.to_string(), "field name", ident.span())?;
                check_field_storage(field, nested)?;
            }
        }
        Fields::Unnamed(unnamed) => {
            for field in &unnamed.unnamed {
                check_field_storage(field, nested)?;
            }
        }
        Fields::Unit => {}
    }
    Ok(())
}

fn check_field_storage(field: &Field, nested: bool) -> syn::Result<()> {
    if nested && let Some(attr) = field.attrs.iter().find(|attr| attr.path().is_ident("storage")) {
        return Err(syn::Error::new_spanned(attr, "field aliases are not supported on nested types"));
    }
    check_alias_names(&super::attr::parse_field_storage(field)?)
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
