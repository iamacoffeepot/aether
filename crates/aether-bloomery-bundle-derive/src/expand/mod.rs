//! Pipeline of the `bundle` export generator: parse, classify, emit, continue.
//!
//! [`generate`] parses the `export!` pipeline state, classifies the exports
//! into the roles the root must serve, emits the root plus each present
//! role's items, and continues the pipeline with every program and reactor
//! replaced by the root once, at the first one's position. When `export!`
//! names no `default`, the root becomes the default, so the multi-actor arm
//! emits the boundary the host matches. With programs present, the
//! invocation actor the root spawns inline joins the `private` list.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Ident, Type};

use aether_bloomery_kinds::BUNDLE_NAMESPACE;

use crate::classify::{Roles, classify};
use crate::input::{Envelope, GenerateInput, NamespaceTok};

mod programs;
mod reactors;
mod root;

const ROOT_IDENT: &str = "__AetherBloomeryBundleRoot";

pub fn generate(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as GenerateInput);
    match expand_generate(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_generate(input: GenerateInput) -> syn::Result<TokenStream2> {
    let roles = classify(&input)?;
    let GenerateInput { remaining_generators, boot, default, actors, exports, private } = input;
    let root = format_ident!("{ROOT_IDENT}");
    let pieces = match &roles {
        Roles::Programs(programs) => vec![programs::pieces(&root, programs)],
        Roles::Reactors(reactors) => vec![reactors::pieces(reactors)],
        Roles::Both { programs, reactors } => {
            vec![programs::pieces(&root, programs), reactors::pieces(reactors)]
        }
    };
    let bundle = root::expand_root(&root, &pieces);
    let boot_tokens = optional_type_tokens(boot.as_ref());
    let default_tokens = default.as_ref().map_or_else(|| quote! { { #root } }, |ty| quote! { { #ty } });
    let actor_tokens = actors.iter().map(envelope_tokens);
    let export_tokens = rewritten_exports(&exports, &roles, &root);
    let private_tokens = listed_private(&private, &roles);
    let rest = remaining_generators.iter();
    Ok(quote! {
        #bundle
        ::aether_actor::__export_continue! {
            remaining_generators: [ #(#rest),* ]
            boot: #boot_tokens
            default: #default_tokens
            actors: [
                #(#actor_tokens)*
                { ty: { #root } namespace: #BUNDLE_NAMESPACE extensions: [] }
            ]
            exports: [ #export_tokens ]
            private: [ #private_tokens ]
        }
    })
}

fn envelope_tokens(entry: &Envelope) -> TokenStream2 {
    let ty = &entry.ty;
    let ns = match &entry.namespace {
        NamespaceTok::Lit(value) => quote! { #value },
        NamespaceTok::Unknown => quote! { _ },
    };
    let ext = &entry.extensions;
    quote! { { ty: { #ty } namespace: #ns extensions: [ #ext ] } }
}

fn rewritten_exports(exports: &[Type], roles: &Roles, root: &Ident) -> TokenStream2 {
    let mut inserted = false;
    let mut out = TokenStream2::new();
    for ty in exports {
        if roles.contains(ty) {
            if !inserted {
                out.extend(quote! { { #root } });
                inserted = true;
            }
        } else {
            out.extend(quote! { { #ty } });
        }
    }
    out
}

/// The pipeline's `private` list, plus the invocation actor when the root
/// spawns one.
fn listed_private(private: &[Type], roles: &Roles) -> TokenStream2 {
    let listed = private.iter().map(|ty| quote! { { #ty } });
    let invocation = matches!(roles, Roles::Programs(_) | Roles::Both { .. })
        .then(programs::invocation_ident)
        .map(|ident| quote! { { #ident } });
    quote! { #(#listed)* #invocation }
}

fn optional_type_tokens(ty: Option<&Type>) -> TokenStream2 {
    ty.map_or_else(|| quote! { none }, |ty| quote! { { #ty } })
}

pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}
