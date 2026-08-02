//! `#[derive(InjectedParams)]` — the ADR-0170 declare-needs derive.
//!
//! Turns a plain struct whose fields are wire kinds into an
//! `aether_actor::InjectedParams` impl: a `REQUESTS` slice naming every
//! requested kind (which `#[actor]` copies into the `aether.kinds.inputs`
//! section) and a `from_entries` that decodes the host's bag field by field.
//!
//! ```ignore
//! #[derive(aether_actor::InjectedParams)]
//! struct ShardParams {
//!     #[param("aether.component.replica_identity")]
//!     replica: ReplicaIdentity,
//! }
//! ```
//!
//! The attribute's kind name is a readable restatement of what the field's
//! type already determines, so the derive pins the two together with a const
//! assertion rather than trusting either alone: a literal that disagrees with
//! `<FieldTy as Kind>::NAME` fails to compile at the declaration.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, LitStr, Type};

/// One `#[param("…")]` field lifted off the struct.
struct ParamField {
    ident: syn::Ident,
    ty: Type,
    declared_name: LitStr,
}

pub fn expand_injected_params(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let self_ty = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "InjectedParams describes a set of named requests, so it derives only on a struct \
             with named fields",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &data.fields,
            "InjectedParams requires named fields — each field name is the request's \
             identity in the manifest and in load errors",
        ));
    };

    let mut fields: Vec<ParamField> = Vec::new();
    for field in &named.named {
        let ident = field.ident.clone().expect("named fields carry an ident");
        let attrs: Vec<&syn::Attribute> = field.attrs.iter().filter(|a| a.path().is_ident("param")).collect();
        match attrs.as_slice() {
            [] => {
                return Err(syn::Error::new(
                    field.span(),
                    "every field of an InjectedParams struct is a host request; annotate it \
                     `#[param(\"<kind name>\")]` (there is no un-injected field — a value the \
                     host does not provide belongs in Config)",
                ));
            }
            [attr] => {
                fields.push(ParamField {
                    ident,
                    ty: field.ty.clone(),
                    declared_name: attr.parse_args::<LitStr>().map_err(|_| {
                        syn::Error::new_spanned(
                            attr,
                            "expected `#[param(\"<kind name>\")]` with a single string literal \
                             naming the requested kind",
                        )
                    })?,
                });
            }
            [_, second, ..] => {
                return Err(syn::Error::new_spanned(
                    second,
                    "a field requests exactly one kind; remove the extra `#[param]`",
                ));
            }
        }
    }

    let field_names: Vec<String> = fields.iter().map(|f| f.ident.to_string()).collect();

    // The declared literal and the field type's own `Kind::NAME` are two
    // spellings of one fact, so hold them together at compile time. The
    // manifest and the decode both read the *type*; the literal exists to
    // make the request legible at the declaration, and this assertion is what
    // stops it from drifting into a lie.
    let name_assertions = fields.iter().zip(&field_names).map(|(f, field_name)| {
        let ty = &f.ty;
        let declared = &f.declared_name;
        let message = format!(
            "#[param] on field `{field_name}` names a different kind than its type declares; \
             the literal must equal <{}>::NAME",
            quote!(#ty),
        );
        quote_spanned_assert(declared, ty, &message)
    });

    let requests = fields.iter().zip(&field_names).map(|(f, field_name)| {
        let ty = &f.ty;
        quote! {
            ::aether_actor::__macro_internals::ParamRequest {
                id: <#ty as ::aether_actor::__macro_internals::Kind>::ID,
                name: <#ty as ::aether_actor::__macro_internals::Kind>::NAME,
                field: #field_name,
            }
        }
    });

    let decodes = fields.iter().zip(&field_names).map(|(f, field_name)| {
        let ident = &f.ident;
        let ty = &f.ty;
        quote! {
            #ident: ::aether_actor::__macro_internals::take_param::<#ty>(__aether_entries, #field_name)?
        }
    });

    Ok(quote! {
        #(#name_assertions)*

        impl #impl_generics ::aether_actor::__macro_internals::InjectedParams
            for #self_ty #ty_generics #where_clause
        {
            const REQUESTS: &'static [::aether_actor::__macro_internals::ParamRequest] = &[#(#requests),*];

            fn from_entries(
                __aether_entries: &[::aether_actor::__macro_internals::ParamEntry],
            ) -> ::core::result::Result<Self, ::aether_actor::__macro_internals::ParamsError> {
                ::core::result::Result::Ok(Self { #(#decodes),* })
            }
        }
    })
}

/// A const assertion that the declared literal equals the field type's
/// `Kind::NAME`, reported at the literal's own span so the diagnostic lands on
/// the wrong spelling rather than on the struct.
fn quote_spanned_assert(declared: &LitStr, ty: &Type, message: &str) -> TokenStream2 {
    let span = declared.span();
    quote::quote_spanned! {span=>
        const _: () = ::core::assert!(
            ::aether_actor::__macro_internals::param_name_matches(
                #declared,
                <#ty as ::aether_actor::__macro_internals::Kind>::NAME,
            ),
            #message,
        );
    }
}
