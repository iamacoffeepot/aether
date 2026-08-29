//! Codegen for `#[derive(Storage)]`.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Data, DataEnum, DeriveInput, Fields, Type};

use super::attr::{TypeStorageAttr, parse_field_storage};
use crate::{KindAttr, is_vec_u8, to_screaming_snake_case};

/// `#[derive(Storage)]` selects the tagged container-element form: the
/// element body is a length-framed record stream rooted at this type,
/// so element schema drift inside a container decodes the way
/// root-level drift does (#5496).
pub(super) fn emit_tagged_element(name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl ::aether_data::__derive_runtime::StorageElement for #name {
            const TAGGED: bool = true;

            fn contribute_element(
                &self,
                depth: u32,
                out: &mut ::aether_data::__derive_runtime::Vec<u8>,
            ) -> ::core::result::Result<(), ::aether_data::__derive_runtime::StorageError> {
                ::aether_data::__derive_runtime::contribute_tagged_element(self, depth, out)
            }

            fn assemble_element(
                depth: u32,
                cursor: &mut &[u8],
            ) -> ::core::result::Result<Self, ::aether_data::__derive_runtime::StorageError> {
                ::aether_data::__derive_runtime::assemble_tagged_element(depth, cursor)
            }
        }
    }
}

pub(super) fn emit(input: &DeriveInput, kind: &KindAttr, storage: &TypeStorageAttr) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let kind_name = &kind.name;
    let strict = storage.strict;
    let leaves = match &input.data {
        Data::Struct(s) => emit_struct_leaves(name, &s.fields)?,
        Data::Enum(e) => emit_enum_leaves(name, e),
        Data::Union(_) => TokenStream2::new(),
    };
    let (alias_statics, alias_pairs) = alias_statics(name, input)?;
    let schema_static = format_ident!("__AETHER_STORAGE_SCHEMA_{}", to_screaming_snake_case(&name.to_string()));
    Ok(quote! {
        impl ::aether_data::Kind for #name {
            const NAME: &'static str = #kind_name;
            const ID: ::aether_data::KindId =
                ::aether_data::__derive_runtime::storage_kind_id_from_name(Self::NAME);

            fn encode_into_bytes(&self) -> ::aether_data::__derive_runtime::Vec<u8> {
                ::core::panic!(
                    "aether-data: Kind::encode_into_bytes called on storage kind `{}`. \
                     Storage values do not have a positional mail codec; they reach mail \
                     only through handle indirection.",
                    Self::NAME,
                )
            }
        }

        impl ::aether_data::Storage for #name {
            const STRICT: bool = #strict;

            fn decode_storage(
                bytes: &[u8],
            ) -> ::core::result::Result<
                ::aether_data::__derive_runtime::StorageData<Self>,
                ::aether_data::__derive_runtime::StorageError,
            > {
                ::aether_data::__derive_runtime::decode_derived(bytes, Self::STRICT)
            }

            fn encode_storage(
                data: &::aether_data::__derive_runtime::StorageData<Self>,
            ) -> ::core::result::Result<
                ::aether_data::__derive_runtime::Vec<u8>,
                ::aether_data::__derive_runtime::StorageError,
            > {
                ::aether_data::__derive_runtime::encode_derived(data)
            }
        }

        #leaves

        #(#alias_statics)*
        static #schema_static: ::aether_data::__derive_runtime::SchemaType =
            <#name as ::aether_data::Schema>::SCHEMA;
        const _: () = ::aether_data::__derive_runtime::assert_unique_storage_leaves(
            &#schema_static,
            &[ #( #alias_pairs ),* ],
        );
    })
}

fn alias_statics(name: &syn::Ident, input: &DeriveInput) -> syn::Result<(Vec<TokenStream2>, Vec<TokenStream2>)> {
    let mut statics = Vec::new();
    let mut pairs = Vec::new();
    let Data::Struct(s) = &input.data else {
        return Ok((statics, pairs));
    };
    let prefix = to_screaming_snake_case(&name.to_string());
    let mut n = 0usize;
    for field in &s.fields {
        let parsed = parse_field_storage(field)?;
        if parsed.aliases.is_empty() {
            continue;
        }
        let ident = format_ident!("__AETHER_STORAGE_ALIAS_{prefix}_{n}");
        n += 1;
        let schema = field_schema_expr(&field.ty);
        statics.push(quote! {
            static #ident: ::aether_data::__derive_runtime::SchemaType = #schema;
        });
        for (alias, _) in parsed.aliases {
            pairs.push(quote! {
                (#alias, &#ident)
            });
        }
    }
    Ok((statics, pairs))
}

fn field_schema_expr(ty: &Type) -> TokenStream2 {
    if is_vec_u8(ty) {
        quote! { ::aether_data::__derive_runtime::BYTES_SCHEMA }
    } else {
        quote! { <#ty as ::aether_data::Schema>::SCHEMA }
    }
}

fn emit_struct_leaves(name: &syn::Ident, fields: &Fields) -> syn::Result<TokenStream2> {
    if matches!(fields, Fields::Unit) || fields.is_empty() {
        return Ok(quote! {
            impl ::aether_data::__derive_runtime::StorageLeaves for #name {
                fn contribute(
                    &self,
                    carry: u64,
                    depth: u32,
                    sink: &mut ::aether_data::__derive_runtime::RecordWriter,
                ) -> ::core::result::Result<(), ::aether_data::__derive_runtime::StorageError> {
                    <() as ::aether_data::__derive_runtime::StorageLeaves>::contribute(&(), carry, depth, sink)
                }

                fn assemble(
                    carry: u64,
                    depth: u32,
                    source: &mut ::aether_data::__derive_runtime::RecordReader,
                ) -> ::core::result::Result<Self, ::aether_data::__derive_runtime::StorageError> {
                    let _: () = <() as ::aether_data::__derive_runtime::StorageLeaves>::assemble(carry, depth, source)?;
                    ::core::result::Result::Ok(Self)
                }

                fn is_absent(
                    carry: u64,
                    depth: u32,
                    source: &::aether_data::__derive_runtime::RecordReader,
                ) -> bool {
                    <() as ::aether_data::__derive_runtime::StorageLeaves>::is_absent(carry, depth, source)
                }
            }
        });
    }

    let contribute_fields = field_contributes(fields);
    let assemble_fields = field_assembles(fields)?;
    let absent_fields = field_absents(fields);
    let ctor = match fields {
        Fields::Named(_) => quote! { Self { #(#assemble_fields),* } },
        Fields::Unnamed(_) => quote! { Self(#(#assemble_fields),*) },
        Fields::Unit => quote! { Self },
    };
    Ok(quote! {
        impl ::aether_data::__derive_runtime::StorageLeaves for #name {
            fn contribute(
                &self,
                carry: u64,
                depth: u32,
                sink: &mut ::aether_data::__derive_runtime::RecordWriter,
            ) -> ::core::result::Result<(), ::aether_data::__derive_runtime::StorageError> {
                #(#contribute_fields)*
                ::core::result::Result::Ok(())
            }

            fn assemble(
                carry: u64,
                depth: u32,
                source: &mut ::aether_data::__derive_runtime::RecordReader,
            ) -> ::core::result::Result<Self, ::aether_data::__derive_runtime::StorageError> {
                ::core::result::Result::Ok(#ctor)
            }

            fn is_absent(
                carry: u64,
                depth: u32,
                source: &::aether_data::__derive_runtime::RecordReader,
            ) -> bool {
                #(#absent_fields)&&*
            }
        }
    })
}

fn field_contributes(fields: &Fields) -> Vec<TokenStream2> {
    let mut out = Vec::new();
    for (idx, field) in fields.iter().enumerate() {
        let fname = match &field.ident {
            Some(id) => id.to_string(),
            None => idx.to_string(),
        };
        let access = if let Some(id) = &field.ident {
            quote!(self.#id)
        } else {
            let index = syn::Index::from(idx);
            quote!(self.#index)
        };
        let child = quote! {
            ::aether_data::__derive_runtime::fold_path_segment(carry, #fname.as_bytes(), depth)
        };
        if is_vec_u8(&field.ty) {
            out.push(quote! {
                ::aether_data::__derive_runtime::contribute_bytes(&#access, #child, depth + 1, sink)?;
            });
        } else {
            out.push(quote! {
                ::aether_data::__derive_runtime::StorageLeaves::contribute(&#access, #child, depth + 1, sink)?;
            });
        }
    }
    out
}

fn field_assembles(fields: &Fields) -> syn::Result<Vec<TokenStream2>> {
    let mut out = Vec::new();
    for (idx, field) in fields.iter().enumerate() {
        let fname = match &field.ident {
            Some(id) => id.to_string(),
            None => idx.to_string(),
        };
        let attr = parse_field_storage(field)?;
        let ty = &field.ty;
        let primary = quote! {
            ::aether_data::__derive_runtime::fold_path_segment(carry, #fname.as_bytes(), depth)
        };
        let alias_carries: Vec<_> = attr
            .aliases
            .iter()
            .map(|(alias, _)| {
                quote! {
                    ::aether_data::__derive_runtime::fold_path_segment(carry, #alias.as_bytes(), depth)
                }
            })
            .collect();
        let value = if is_vec_u8(ty) {
            quote! {
                ::aether_data::__derive_runtime::assemble_bytes_with_aliases(
                    #primary,
                    &[#(#alias_carries),*],
                    depth + 1,
                    source,
                )?
            }
        } else {
            quote! {
                ::aether_data::__derive_runtime::assemble_with_aliases::<#ty>(
                    #primary,
                    &[#(#alias_carries),*],
                    depth + 1,
                    source,
                )?
            }
        };
        if let Some(ident) = &field.ident {
            out.push(quote! { #ident: #value });
        } else {
            out.push(value);
        }
    }
    Ok(out)
}

fn field_absents(fields: &Fields) -> Vec<TokenStream2> {
    let mut out = Vec::new();
    for (idx, field) in fields.iter().enumerate() {
        let fname = match &field.ident {
            Some(id) => id.to_string(),
            None => idx.to_string(),
        };
        let ty = &field.ty;
        let child = quote! {
            ::aether_data::__derive_runtime::fold_path_segment(carry, #fname.as_bytes(), depth)
        };
        if is_vec_u8(ty) {
            out.push(quote! {
                ::aether_data::__derive_runtime::bytes_absent(#child, source)
            });
        } else {
            out.push(quote! {
                <#ty as ::aether_data::__derive_runtime::StorageLeaves>::is_absent(#child, depth + 1, source)
            });
        }
    }
    out
}

fn emit_enum_leaves(name: &syn::Ident, data: &DataEnum) -> TokenStream2 {
    let mut disc_consts = Vec::new();
    let mut contribute_arms = Vec::new();
    let mut assemble_arms = Vec::new();
    for variant in &data.variants {
        let vident = &variant.ident;
        let vname = vident.to_string();
        let disc_ident = format_ident!("__AETHER_STORAGE_VAR_{}", vname.to_uppercase());
        let body_schema = variant_body_schema(&variant.fields);
        disc_consts.push(quote! {
            const #disc_ident: u64 = ::aether_data::__derive_runtime::variant_hash(#vname, &#body_schema);
        });
        contribute_arms.push(enum_contribute_arm(vident, &vname, &disc_ident, &variant.fields));
        assemble_arms.push(enum_assemble_arm(vident, &vname, &disc_ident, &variant.fields));
    }
    quote! {
        impl ::aether_data::__derive_runtime::StorageLeaves for #name {
            fn contribute(
                &self,
                carry: u64,
                depth: u32,
                sink: &mut ::aether_data::__derive_runtime::RecordWriter,
            ) -> ::core::result::Result<(), ::aether_data::__derive_runtime::StorageError> {
                #(#disc_consts)*
                let __var_carry = ::aether_data::__derive_runtime::fold_path_segment(
                    carry,
                    ::aether_data::__derive_runtime::VARIANT_LEAF.as_bytes(),
                    depth,
                );
                match self {
                    #(#contribute_arms)*
                }
            }

            fn assemble(
                carry: u64,
                depth: u32,
                source: &mut ::aether_data::__derive_runtime::RecordReader,
            ) -> ::core::result::Result<Self, ::aether_data::__derive_runtime::StorageError> {
                #(#disc_consts)*
                let __var_carry = ::aether_data::__derive_runtime::fold_path_segment(
                    carry,
                    ::aether_data::__derive_runtime::VARIANT_LEAF.as_bytes(),
                    depth,
                );
                let __disc: u64 = <u64 as ::aether_data::__derive_runtime::StorageLeaves>::assemble(
                    __var_carry,
                    depth + 1,
                    source,
                )?;
                #(#assemble_arms)*
                ::core::result::Result::Err(
                    ::aether_data::__derive_runtime::StorageError::UnknownVariant { hash: __disc },
                )
            }

            fn is_absent(
                carry: u64,
                depth: u32,
                source: &::aether_data::__derive_runtime::RecordReader,
            ) -> bool {
                let __var_carry = ::aether_data::__derive_runtime::fold_path_segment(
                    carry,
                    ::aether_data::__derive_runtime::VARIANT_LEAF.as_bytes(),
                    depth,
                );
                <u64 as ::aether_data::__derive_runtime::StorageLeaves>::is_absent(__var_carry, depth + 1, source)
            }
        }
    }
}

fn variant_body_schema(fields: &Fields) -> TokenStream2 {
    match fields {
        Fields::Unit => quote! { ::aether_data::__derive_runtime::UNIT_SCHEMA },
        Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
            let ty = &unnamed.unnamed[0].ty;
            field_schema_expr(ty)
        }
        Fields::Unnamed(unnamed) => {
            struct_schema_from_fields(unnamed.unnamed.iter().enumerate().map(|(i, f)| (i.to_string(), &f.ty)))
        }
        Fields::Named(named) => struct_schema_from_fields(
            named.named.iter().map(|f| (f.ident.as_ref().map(ToString::to_string).unwrap_or_default(), &f.ty)),
        ),
    }
}

fn struct_schema_from_fields<'a>(fields: impl Iterator<Item = (String, &'a Type)>) -> TokenStream2 {
    let entries = fields.map(|(fname, ty)| {
        let schema = field_schema_expr(ty);
        quote! {
            ::aether_data::__derive_runtime::NamedField {
                name: ::aether_data::__derive_runtime::Cow::Borrowed(#fname),
                ty: #schema,
            }
        }
    });
    quote! {
        ::aether_data::__derive_runtime::SchemaType::Struct {
            fields: ::aether_data::__derive_runtime::Cow::Borrowed(&[#(#entries),*]),
            repr_c: false,
        }
    }
}

fn enum_contribute_arm(vident: &syn::Ident, vname: &str, disc_ident: &syn::Ident, fields: &Fields) -> TokenStream2 {
    let disc_emit = quote! {
        <u64 as ::aether_data::__derive_runtime::StorageLeaves>::contribute(
            &#disc_ident, __var_carry, depth + 1, sink
        )?;
    };
    let body_carry = quote! {
        ::aether_data::__derive_runtime::fold_path_segment(carry, #vname.as_bytes(), depth)
    };
    match fields {
        Fields::Unit => quote! {
            Self::#vident => {
                #disc_emit
                ::core::result::Result::Ok(())
            }
        },
        Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
            let ty = &unnamed.unnamed[0].ty;
            let inner = if is_vec_u8(ty) {
                quote! {
                    ::aether_data::__derive_runtime::contribute_bytes(__inner, #body_carry, depth + 1, sink)?;
                }
            } else {
                quote! {
                    ::aether_data::__derive_runtime::StorageLeaves::contribute(__inner, #body_carry, depth + 1, sink)?;
                }
            };
            quote! {
                Self::#vident(__inner) => {
                    #disc_emit
                    #inner
                    ::core::result::Result::Ok(())
                }
            }
        }
        Fields::Unnamed(unnamed) => {
            let bindings: Vec<_> = (0..unnamed.unnamed.len()).map(|i| format_ident!("__f{i}")).collect();
            let contributes = unnamed.unnamed.iter().enumerate().map(|(i, f)| {
                let binding = format_ident!("__f{i}");
                let idx = i.to_string();
                let child = quote! {
                    ::aether_data::__derive_runtime::fold_path_segment(
                        #body_carry,
                        #idx.as_bytes(),
                        depth + 1,
                    )
                };
                if is_vec_u8(&f.ty) {
                    quote! {
                        ::aether_data::__derive_runtime::contribute_bytes(#binding, #child, depth + 2, sink)?;
                    }
                } else {
                    quote! {
                        ::aether_data::__derive_runtime::StorageLeaves::contribute(#binding, #child, depth + 2, sink)?;
                    }
                }
            });
            quote! {
                Self::#vident(#(#bindings),*) => {
                    #disc_emit
                    #(#contributes)*
                    ::core::result::Result::Ok(())
                }
            }
        }
        Fields::Named(named) => {
            let idents: Vec<_> = named.named.iter().filter_map(|f| f.ident.as_ref()).collect();
            let contributes = named.named.iter().map(|f| {
                let ident = f.ident.as_ref();
                let fname = ident.map(ToString::to_string).unwrap_or_default();
                let child = quote! {
                    ::aether_data::__derive_runtime::fold_path_segment(
                        #body_carry,
                        #fname.as_bytes(),
                        depth + 1,
                    )
                };
                if is_vec_u8(&f.ty) {
                    quote! {
                        ::aether_data::__derive_runtime::contribute_bytes(#ident, #child, depth + 2, sink)?;
                    }
                } else {
                    quote! {
                        ::aether_data::__derive_runtime::StorageLeaves::contribute(#ident, #child, depth + 2, sink)?;
                    }
                }
            });
            quote! {
                Self::#vident { #(#idents),* } => {
                    #disc_emit
                    #(#contributes)*
                    ::core::result::Result::Ok(())
                }
            }
        }
    }
}

fn enum_assemble_arm(vident: &syn::Ident, vname: &str, disc_ident: &syn::Ident, fields: &Fields) -> TokenStream2 {
    let body_carry = quote! {
        ::aether_data::__derive_runtime::fold_path_segment(carry, #vname.as_bytes(), depth)
    };
    match fields {
        Fields::Unit => quote! {
            if __disc == #disc_ident {
                return ::core::result::Result::Ok(Self::#vident);
            }
        },
        Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
            let ty = &unnamed.unnamed[0].ty;
            let inner = if is_vec_u8(ty) {
                quote! {
                    ::aether_data::__derive_runtime::assemble_bytes(#body_carry, depth + 1, source)?
                }
            } else {
                quote! {
                    <#ty as ::aether_data::__derive_runtime::StorageLeaves>::assemble(#body_carry, depth + 1, source)?
                }
            };
            quote! {
                if __disc == #disc_ident {
                    return ::core::result::Result::Ok(Self::#vident(#inner));
                }
            }
        }
        Fields::Unnamed(unnamed) => {
            let values = unnamed.unnamed.iter().enumerate().map(|(i, f)| {
                let idx = i.to_string();
                let child = quote! {
                    ::aether_data::__derive_runtime::fold_path_segment(
                        #body_carry,
                        #idx.as_bytes(),
                        depth + 1,
                    )
                };
                if is_vec_u8(&f.ty) {
                    quote! { ::aether_data::__derive_runtime::assemble_bytes(#child, depth + 2, source)? }
                } else {
                    let ty = &f.ty;
                    quote! {
                        <#ty as ::aether_data::__derive_runtime::StorageLeaves>::assemble(#child, depth + 2, source)?
                    }
                }
            });
            quote! {
                if __disc == #disc_ident {
                    return ::core::result::Result::Ok(Self::#vident(#(#values),*));
                }
            }
        }
        Fields::Named(named) => {
            let values = named.named.iter().map(|f| {
                let ident = f.ident.as_ref();
                let fname = ident.map(ToString::to_string).unwrap_or_default();
                let child = quote! {
                    ::aether_data::__derive_runtime::fold_path_segment(
                        #body_carry,
                        #fname.as_bytes(),
                        depth + 1,
                    )
                };
                let value = if is_vec_u8(&f.ty) {
                    quote! { ::aether_data::__derive_runtime::assemble_bytes(#child, depth + 2, source)? }
                } else {
                    let ty = &f.ty;
                    quote! {
                        <#ty as ::aether_data::__derive_runtime::StorageLeaves>::assemble(#child, depth + 2, source)?
                    }
                };
                quote! { #ident: #value }
            });
            quote! {
                if __disc == #disc_ident {
                    return ::core::result::Result::Ok(Self::#vident { #(#values),* });
                }
            }
        }
    }
}
