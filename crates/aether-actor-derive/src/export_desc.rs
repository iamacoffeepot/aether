//! Framework-owned export descriptor companion for `export!(..., generators = […])`.
//!
//! `#[actor]` emits a hidden `#[macro_export]` macro with a unique crate-root
//! name, then `pub use`s it as the type's ident so `use`, `pub use`, and
//! `as` aliases carry the companion in the macro namespace. The envelope is
//! actor metadata plus optional namespaced extensions; ordinary actors emit
//! an empty extension list. `type Alias = T` is not followed.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Expr, ExprLit, Ident, Lit, Type};

pub fn emit_actor_export_desc(self_ty: &Type, namespace: &Expr) -> TokenStream2 {
    let Some(ident) = type_last_ident(self_ty) else {
        return quote! {};
    };
    emit_export_desc_companion(ident, &namespace_token(namespace), &quote! {})
}

fn emit_export_desc_companion(ident: &Ident, namespace: &TokenStream2, extensions: &TokenStream2) -> TokenStream2 {
    let unique = unique_macro_ident(ident);
    quote! {
        #[doc(hidden)]
        #[macro_export]
        macro_rules! #unique {
            (@aether_export_desc $__aether_got:path { $($__aether_state:tt)* }) => {
                $__aether_got! {
                    @aether_export_got
                    { namespace: #namespace, extensions: [ #extensions ] }
                    { $($__aether_state)* }
                }
            };
        }
        #[doc(hidden)]
        pub use #unique as #ident;
        #ident! {
            @aether_export_desc
            ::aether_actor::__export_desc_discard
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

fn namespace_token(expr: &Expr) -> TokenStream2 {
    if let Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) = peel_group(expr) {
        quote! { #value }
    } else {
        quote! { _ }
    }
}

fn peel_group(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group(group) => peel_group(&group.expr),
        other => other,
    }
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
