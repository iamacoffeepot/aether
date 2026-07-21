//! `#[derive(aether_substrate::StageArgv)]` — the container half of the
//! ADR-0156 §5 argv-staging inversion (issue 3872).
//!
//! The hand-written chassis CLI roots (`CommonOverlay`, `DesktopCli`,
//! `HeadlessCli`, `HubCli`) stay static clap structs — they model cross-cap
//! chassis shape the `Config` derive deliberately doesn't try to express — but
//! their argv staging is now derived rather than hand-maintained. This derive
//! emits a `StageArgv` impl that delegates to every field's own `stage`:
//!
//! - A cap overlay field (`http: HttpOverlay`) forwards to the leaf `StageArgv`
//!   the `Config` derive emitted on it.
//! - A nested container field (`common: CommonOverlay`) forwards to that
//!   container's own derived `StageArgv`.
//! - A non-stageable field (`config` / `print_config` / `describe`) must carry
//!   an explicit `#[stage(skip)]`. An unannotated field whose type does not
//!   implement `StageArgv` is a hard compile error at the delegating call — the
//!   hole the derive exists to close cannot reopen through the derive itself.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Data, DataStruct, DeriveInput, Fields, parse_macro_input};

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(syn::Error::new_spanned(ident, "`#[derive(StageArgv)]` only supports structs with named fields"));
    };
    let Fields::Named(named) = fields else {
        return Err(syn::Error::new_spanned(fields, "`#[derive(StageArgv)]` only supports structs with named fields"));
    };

    let mut stmts: Vec<TokenStream2> = Vec::new();
    for field in &named.named {
        if has_stage_skip(&field.attrs)? {
            continue;
        }
        let field_ident = field.ident.as_ref().expect("named field checked above");
        // Delegate to the field's own `StageArgv::stage`. A field whose type
        // does not implement `StageArgv` (and carries no `#[stage(skip)]`) fails
        // to compile here — never a silent skip.
        stmts.push(quote! {
            ::aether_substrate::config::StageArgv::stage(self.#field_ident, sources);
        });
    }

    Ok(quote! {
        impl ::aether_substrate::config::StageArgv for #ident {
            fn stage(self, sources: &mut ::aether_substrate::config::ConfigSources) {
                #( #stmts )*
            }
        }
    })
}

/// Whether a field carries `#[stage(skip)]`. Any other `#[stage(...)]` content
/// is a hard error so a typo is loud rather than silently treated as non-skip.
fn has_stage_skip(attrs: &[Attribute]) -> syn::Result<bool> {
    let mut skip = false;
    for attr in attrs {
        if !attr.path().is_ident("stage") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                skip = true;
                Ok(())
            } else {
                Err(meta.error("unknown `stage` attribute; expected `skip`"))
            }
        })?;
    }
    Ok(skip)
}
