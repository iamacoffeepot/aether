//! `#[derive(StorageLeaf)]` — a fixed-layout value type as one storage leaf.
//!
//! The type keeps `#[derive(Schema)]`, which gives it the positional wire
//! codec and the positional container element. This derive adds the two
//! impls a `Storage` field still needs: `StorageLeaves` as one opaque
//! record — tagged by the field path and the type's schema, its body the
//! type's ordinary wire bytes, exactly as a scalar is stored — and an
//! empty `Cites`. The record carries no tolerance promise: changing the
//! type's shape moves the tag, and old rows refuse by name.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput};

pub fn expand_storage_leaf(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if let Data::Union(u) = &input.data {
        return Err(syn::Error::new_spanned(u.union_token, "StorageLeaf derive does not support unions"));
    }
    let name = &input.ident;
    Ok(quote! {
        impl ::aether_data::__derive_runtime::StorageLeaves for #name {
            fn contribute(
                &self,
                carry: u64,
                depth: u32,
                sink: &mut ::aether_data::__derive_runtime::RecordWriter,
            ) -> ::core::result::Result<(), ::aether_data::__derive_runtime::StorageError> {
                ::aether_data::__derive_runtime::contribute_opaque(self, carry, depth, sink)
            }

            fn assemble(
                carry: u64,
                depth: u32,
                source: &mut ::aether_data::__derive_runtime::RecordReader,
            ) -> ::core::result::Result<Self, ::aether_data::__derive_runtime::StorageError> {
                ::aether_data::__derive_runtime::assemble_opaque(carry, depth, source)
            }

            fn is_absent(
                carry: u64,
                _depth: u32,
                source: &::aether_data::__derive_runtime::RecordReader,
            ) -> bool {
                ::aether_data::__derive_runtime::opaque_absent::<Self>(carry, source)
            }
        }

        impl ::aether_data::__derive_runtime::Cites for #name {
            fn cites(&self, _sink: &mut ::aether_data::__derive_runtime::Citations) {}
        }
    })
}
