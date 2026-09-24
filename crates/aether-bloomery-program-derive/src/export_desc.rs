//! Bloomery extension on the framework-owned export descriptor envelope.
//!
//! `#[program]` emits the same companion shape as `#[actor]`, with an
//! `aether_bloomery_program { … }` extension. `aether_bloomery_bundle::bundle` selects that
//! extension; it does not own actor-vs-program discovery.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Ident, Type};

use crate::parse::ProgramDef;

pub fn emit_program_export_desc(def: &ProgramDef) -> TokenStream2 {
    let Some(ident) = type_last_ident(&def.self_ty) else {
        return quote! {};
    };
    let unique = unique_macro_ident(ident);
    let discard = format_ident!("{unique}_discard");
    let async_run = syn::LitBool::new(def.async_run, proc_macro2::Span::call_site());
    let mode = if def.sampled {
        format_ident!("Sampled")
    } else {
        format_ident!("Pure")
    };
    let name = &def.name;
    let intent = &def.intent;
    let input = &def.input;
    let result = &def.result;
    let api_names = def.apis.iter().map(|api| &api.name);
    quote! {
        #[doc(hidden)]
        #[macro_export]
        macro_rules! #unique {
            (@aether_export_desc $__aether_got:path { $($__aether_state:tt)* }) => {
                $__aether_got! {
                    @aether_export_got
                    {
                        namespace: _,
                        extensions: [
                            aether_bloomery_program {
                                name: #name,
                                intent: #intent,
                                mode: #mode,
                                input: #input,
                                result: #result,
                                async_run: #async_run,
                                apis: [#(#api_names),*],
                            }
                        ]
                    }
                    { $($__aether_state)* }
                }
            };
        }
        #[doc(hidden)]
        pub use #unique as #ident;
        #[doc(hidden)]
        macro_rules! #discard {
            (@aether_export_got { $($__aether_meta:tt)* } { $($__aether_state:tt)* }) => {};
        }
        #ident! {
            @aether_export_desc
            #discard
            {}
        }
    }
}

fn unique_macro_ident(name: &Ident) -> Ident {
    let key = if proc_macro::is_available() {
        let span = name.span().unwrap();
        format!("{}:{}:{}:{name}", span.file(), span.line(), span.column())
    } else {
        format!("{:?}:{name}", name.span())
    };
    format_ident!("__aether_export_desc_{}_{:x}", name, fnv1a_64(key.as_bytes()))
}

fn type_last_ident(ty: &Type) -> Option<&Ident> {
    match ty {
        Type::Path(path) => path.path.segments.last().map(|segment| &segment.ident),
        Type::Group(group) => type_last_ident(&group.elem),
        Type::Paren(paren) => type_last_ident(&paren.elem),
        _ => None,
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}
