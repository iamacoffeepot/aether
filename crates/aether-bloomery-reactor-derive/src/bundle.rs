//! `bundle_reactors` export generator: select bloomery reactor extensions from
//! the framework-owned descriptor list, emit one digest-loaded root, then
//! continue the `export!` generator pipeline.

use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, LitStr, Path, Token, Type, braced, bracketed};

use crate::export_desc::fnv1a_64;

// Keep in lockstep with `aether_bloomery_reactor::REACTOR_NAMESPACE`. The derive
// crate cannot read that const (runtime → derive dependency), so reserved-namespace
// detection compares against this copy.
const REACTOR_NAMESPACE: &str = "aether.bloomery.reactor";
const ROOT_IDENT: &str = "__AetherBloomeryReactorRoot";
const REACTOR_EXTENSION: &str = "aether_bloomery_reactor";

pub fn generate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(input as GenerateInput);
    match expand_generate(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

struct GenerateInput {
    remaining_generators: Vec<Path>,
    boot: Option<Type>,
    default: Option<Type>,
    actors: Vec<Envelope>,
    exports: Vec<Type>,
}

struct Envelope {
    ty: Type,
    namespace: NamespaceTok,
    extensions: TokenStream2,
    is_reactor: bool,
}

enum NamespaceTok {
    Lit(String),
    Unknown,
}

impl Parse for GenerateInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut remaining_generators = Vec::new();
        let mut boot = None;
        let mut default = None;
        let mut actors = Vec::new();
        let mut exports = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "remaining_generators" => remaining_generators = parse_path_list(input)?,
                "boot" => boot = parse_optional_type(input)?,
                "default" => default = parse_optional_type(input)?,
                "actors" => actors = parse_classified_list(input)?,
                "exports" => exports = parse_export_types(input)?,
                other => return Err(syn::Error::new_spanned(&key, format!("unknown generator field `{other}`"))),
            }
        }
        if exports.is_empty() {
            return Err(syn::Error::new(Span::call_site(), "bundle_reactors requires at least one export type"));
        }
        Ok(Self { remaining_generators, boot, default, actors, exports })
    }
}

fn parse_path_list(input: ParseStream<'_>) -> syn::Result<Vec<Path>> {
    let content;
    bracketed!(content in input);
    let paths = Punctuated::<Path, Token![,]>::parse_terminated(&content)?;
    Ok(paths.into_iter().collect())
}

fn parse_optional_type(input: ParseStream<'_>) -> syn::Result<Option<Type>> {
    if input.peek(Ident) {
        let ident: Ident = input.fork().parse()?;
        if ident == "none" {
            input.parse::<Ident>()?;
            return Ok(None);
        }
    }
    let content;
    braced!(content in input);
    Ok(Some(content.parse()?))
}

fn parse_classified_list(input: ParseStream<'_>) -> syn::Result<Vec<Envelope>> {
    let content;
    bracketed!(content in input);
    let mut classified = Vec::new();
    while !content.is_empty() {
        let wrapped;
        braced!(wrapped in content);
        classified.push(parse_envelope(&wrapped)?);
    }
    Ok(classified)
}

fn parse_export_types(input: ParseStream<'_>) -> syn::Result<Vec<Type>> {
    let content;
    bracketed!(content in input);
    let mut types = Vec::new();
    while !content.is_empty() {
        let wrapped;
        braced!(wrapped in content);
        types.push(wrapped.parse()?);
    }
    Ok(types)
}

fn parse_envelope(input: ParseStream<'_>) -> syn::Result<Envelope> {
    let mut ty = None;
    let mut namespace = NamespaceTok::Unknown;
    let mut extensions = TokenStream2::new();
    let mut is_reactor = false;
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        match key.to_string().as_str() {
            "ty" => {
                let inner;
                braced!(inner in input);
                ty = Some(inner.parse()?);
            }
            "namespace" => namespace = parse_namespace(input)?,
            "extensions" => {
                let inner;
                bracketed!(inner in input);
                let parsed = parse_extensions(&inner)?;
                is_reactor = parsed.0;
                extensions = parsed.1;
            }
            other => return Err(syn::Error::new_spanned(&key, format!("unknown classified field `{other}`"))),
        }
    }
    let ty = ty.ok_or_else(|| syn::Error::new(Span::call_site(), "actor envelope missing ty"))?;
    Ok(Envelope { ty, namespace, extensions, is_reactor })
}

fn parse_namespace(input: ParseStream<'_>) -> syn::Result<NamespaceTok> {
    if input.peek(LitStr) {
        let value: LitStr = input.parse()?;
        Ok(NamespaceTok::Lit(value.value()))
    } else if input.peek(Token![_]) {
        input.parse::<Token![_]>()?;
        Ok(NamespaceTok::Unknown)
    } else {
        Err(input.error("classified namespace must be a string literal or `_`"))
    }
}

fn parse_extensions(input: ParseStream<'_>) -> syn::Result<(bool, TokenStream2)> {
    let mut is_reactor = false;
    let mut tokens = TokenStream2::new();
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        if key == REACTOR_EXTENSION {
            is_reactor = true;
        }
        let payload;
        braced!(payload in input);
        let mut payload_tokens = TokenStream2::new();
        while !payload.is_empty() {
            let tt: TokenTree = payload.parse()?;
            payload_tokens.extend([tt]);
        }
        tokens.extend(quote! { #key { #payload_tokens } });
    }
    Ok((is_reactor, tokens))
}

fn expand_generate(input: GenerateInput) -> syn::Result<TokenStream2> {
    let GenerateInput { remaining_generators, boot, default, actors, exports } = input;
    for entry in &actors {
        if let NamespaceTok::Lit(namespace) = &entry.namespace
            && namespace == REACTOR_NAMESPACE
        {
            return Err(syn::Error::new_spanned(
                &entry.ty,
                format!("NAMESPACE `{namespace}` is reserved for the generated reactor root"),
            ));
        }
    }

    let mut reactors = Vec::new();
    let mut seen_ns = Vec::new();
    for export_ty in &exports {
        let Some(entry) = actors.iter().find(|entry| types_eq(export_ty, &entry.ty)) else {
            continue;
        };
        if !entry.is_reactor {
            continue;
        }
        let NamespaceTok::Lit(namespace) = &entry.namespace else {
            return Err(syn::Error::new_spanned(&entry.ty, "reactor NAMESPACE must be a string literal"));
        };
        if seen_ns.iter().any(|existing| existing == namespace) {
            return Err(syn::Error::new_spanned(&entry.ty, format!("duplicate reactor NAMESPACE `{namespace}`")));
        }
        seen_ns.push(namespace.clone());
        reactors.push(entry);
    }
    if reactors.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "bundle_reactors found no #[reactor] exports in this export! set",
        ));
    }
    if let Some(boot) = &boot
        && type_in(boot, reactors.iter().map(|entry| &entry.ty))
    {
        return Err(syn::Error::new_spanned(boot, "export! boot type cannot be a reactor"));
    }
    let mixed = exports.iter().any(|ty| !type_in(ty, reactors.iter().map(|entry| &entry.ty)));
    if let Some(default) = &default
        && type_in(default, reactors.iter().map(|entry| &entry.ty))
    {
        return Err(syn::Error::new_spanned(
            default,
            if mixed {
                "export! default cannot be a reactor in a mixed module; name an ordinary actor or omit default"
            } else {
                "export! default cannot be a reactor; a reactor-only module exports the generated reactor root"
            },
        ));
    }

    let root = format_ident!("{ROOT_IDENT}");
    let generated = expand_root(&root, &reactors);
    let sections = reactors.iter().copied().map(expand_section);
    let boot_tokens = optional_type_tokens(boot.as_ref());
    let default_tokens = default.as_ref().map_or_else(|| quote! { { #root } }, |ty| quote! { { #ty } });
    let actor_tokens = actors.iter().map(envelope_tokens);
    let export_tokens = rewritten_exports(&exports, &reactors, &root);
    let rest = remaining_generators.iter();
    Ok(quote! {
        #generated
        #(#sections)*
        ::aether_actor::__export_continue! {
            remaining_generators: [ #(#rest),* ]
            boot: #boot_tokens
            default: #default_tokens
            actors: [
                #(#actor_tokens)*
                { ty: { #root } namespace: #REACTOR_NAMESPACE extensions: [] }
            ]
            exports: [ #export_tokens ]
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

fn rewritten_exports(exports: &[Type], reactors: &[&Envelope], coordinator: &Ident) -> TokenStream2 {
    let mut inserted = false;
    let mut out = TokenStream2::new();
    for ty in exports {
        if type_in(ty, reactors.iter().map(|entry| &entry.ty)) {
            if !inserted {
                out.extend(quote! { { #coordinator } });
                inserted = true;
            }
        } else {
            out.extend(quote! { { #ty } });
        }
    }
    out
}

fn optional_type_tokens(ty: Option<&Type>) -> TokenStream2 {
    ty.map_or_else(|| quote! { none }, |ty| quote! { { #ty } })
}

fn type_in<'a>(needle: &Type, haystack: impl IntoIterator<Item = &'a Type>) -> bool {
    haystack.into_iter().any(|ty| types_eq(needle, ty))
}

fn types_eq(left: &Type, right: &Type) -> bool {
    quote!(#left).to_string() == quote!(#right).to_string()
}

fn expand_root(root: &Ident, reactors: &[&Envelope]) -> TokenStream2 {
    let mut list = quote! { ::aether_bloomery_reactor::Nil };
    for entry in reactors.iter().rev() {
        let ty = &entry.ty;
        list = quote! { (#ty, #list) };
    }
    quote! {
        struct #root {
            inner: ::aether_bloomery_reactor::Root<#list>,
        }

        #[::aether_actor::actor]
        impl ::aether_actor::WasmActor for #root {
            const NAMESPACE: &'static str = ::aether_bloomery_reactor::REACTOR_NAMESPACE;

            fn init(
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                match ::aether_bloomery_reactor::Root::new() {
                    ::core::result::Result::Ok(inner) => ::core::result::Result::Ok(Self { inner }),
                    ::core::result::Result::Err(reason) => ::core::result::Result::Err(
                        ::aether_actor::ActorInitError::new(
                            ::aether_bloomery_reactor::__macro_internals::ToString::to_string(reason.as_str()),
                        ),
                    ),
                }
            }

            #[handler::manual]
            fn on_warm(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                warm: ::aether_bloomery_reactor::kinds::Warm,
            ) {
                use ::aether_actor::OutboundReply;
                if ctx.reply_target().is_none() {
                    ::aether_actor::__macro_internals::tracing::warn!(
                        "reactor root ignored a request with no reply target"
                    );
                    return;
                }
                ctx.reply(&self.inner.warm(warm));
            }

            #[handler::manual]
            fn on_event(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                event: ::aether_bloomery_reactor::kinds::Event,
            ) {
                use ::aether_actor::OutboundReply;
                if ctx.reply_target().is_none() {
                    ::aether_actor::__macro_internals::tracing::warn!(
                        "reactor root ignored a request with no reply target"
                    );
                    return;
                }
                ctx.reply(&self.inner.event(event));
            }

            #[handler::manual]
            fn on_status(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                _query: ::aether_bloomery_reactor::kinds::StatusQuery,
            ) {
                use ::aether_actor::OutboundReply;
                if ctx.reply_target().is_none() {
                    ::aether_actor::__macro_internals::tracing::warn!(
                        "reactor root ignored a request with no reply target"
                    );
                    return;
                }
                ctx.reply(&self.inner.status());
            }
        }
    }
}

// The link-section name below stays in lockstep with
// `aether_bloomery_kinds::REACTORS_SECTION`. The derive crate cannot read that
// const (runtime → derive dependency), so the literal is copied here, as
// `REACTOR_NAMESPACE` is above.
fn expand_section(entry: &Envelope) -> TokenStream2 {
    let ty = &entry.ty;
    let NamespaceTok::Lit(namespace) = &entry.namespace else {
        return TokenStream2::new();
    };
    let key = format!("{namespace}:{}", quote!(#ty));
    let hash = fnv1a_64(key.as_bytes());
    let len_ident = format_ident!("__AETHER_BLOOMERY_REACTOR_SECTION_LEN_{hash:016X}");
    let bytes_ident = format_ident!("__AETHER_BLOOMERY_REACTOR_SECTION_BYTES_{hash:016X}");
    let section_ident = format_ident!("__AETHER_BLOOMERY_REACTOR_SECTION_{hash:016X}");
    quote! {
        const #len_ident: usize = <#ty as ::aether_bloomery_reactor::Reactor>::DECLARATION.len();
        const #bytes_ident: [u8; #len_ident] =
            ::aether_bloomery_reactor::__macro_internals::record_array::<#len_ident>(
                <#ty as ::aether_bloomery_reactor::Reactor>::DECLARATION,
            );
        const _: &[u8] = &#bytes_ident;
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = "aether.bloomery.reactors")]
        static #section_ident: [u8; #len_ident] = #bytes_ident;
    }
}
