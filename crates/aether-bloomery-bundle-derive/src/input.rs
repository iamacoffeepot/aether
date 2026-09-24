//! Grammar of the `bundle` generator's input: the `export!` pipeline state.
//!
//! `export!` hands every generator the same shape: the remaining generator
//! paths, the optional `boot` / `default` types, one framework-owned
//! descriptor envelope per listed path, the current `exports` selection, and
//! the `private` inline-child types.
//! Each envelope's extensions decide its [`Tag`]: an `aether_bloomery_program`
//! extension carries a [`ProgramMeta`], an `aether_bloomery_reactor` extension
//! marks a reactor, and anything else is ordinary. Every extension's tokens
//! pass through unchanged for the rest of the pipeline.

use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, LitBool, LitStr, Path, Token, Type, braced, bracketed};

const PROGRAM_EXTENSION: &str = "aether_bloomery_program";
const REACTOR_EXTENSION: &str = "aether_bloomery_reactor";

pub struct GenerateInput {
    pub remaining_generators: Vec<Path>,
    pub boot: Option<Type>,
    pub default: Option<Type>,
    pub actors: Vec<Envelope>,
    pub exports: Vec<Type>,
    pub private: Vec<Type>,
}

pub struct Envelope {
    pub ty: Type,
    pub namespace: NamespaceTok,
    pub extensions: TokenStream2,
    pub tag: Tag,
}

pub enum Tag {
    Ordinary,
    Program(Box<ProgramMeta>),
    Reactor,
}

#[derive(Clone)]
pub struct ProgramMeta {
    pub name: LitStr,
    pub intent: LitStr,
    pub async_run: bool,
    pub sampled: bool,
    /// Canonical API names (`Http`, `Process`): each resolves through the
    /// SDK's `__macro_internals::api_target` table.
    pub apis: Vec<Ident>,
}

pub enum NamespaceTok {
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
        let mut private = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "remaining_generators" => remaining_generators = parse_path_list(input)?,
                "boot" => boot = parse_optional_type(input)?,
                "default" => default = parse_optional_type(input)?,
                "actors" => actors = parse_classified_list(input)?,
                "exports" => exports = parse_export_types(input)?,
                "private" => private = parse_export_types(input)?,
                other => return Err(syn::Error::new_spanned(&key, format!("unknown generator field `{other}`"))),
            }
        }
        if exports.is_empty() {
            return Err(syn::Error::new(Span::call_site(), "bundle requires at least one export type"));
        }
        Ok(Self { remaining_generators, boot, default, actors, exports, private })
    }
}

impl Parse for ProgramMeta {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut intent = None;
        let mut async_run = false;
        let mut sampled = false;
        let mut apis = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "name" => {
                    let value: LitStr = input.parse()?;
                    name = Some(value);
                }
                "intent" => {
                    let value: LitStr = input.parse()?;
                    intent = Some(value);
                }
                "mode" => {
                    let mode: Ident = input.parse()?;
                    if mode == "Pure" {
                        sampled = false;
                    } else if mode == "Sampled" {
                        sampled = true;
                    } else {
                        return Err(syn::Error::new_spanned(mode, "bundle requires Mode::Pure or Mode::Sampled"));
                    }
                }
                "apis" => {
                    let content;
                    syn::bracketed!(content in input);
                    let names = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?;
                    apis = names.into_iter().collect();
                }
                "async_run" => {
                    let value: LitBool = input.parse()?;
                    async_run = value.value();
                }
                other => {
                    return Err(syn::Error::new_spanned(&key, format!("unknown program extension field `{other}`")));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| syn::Error::new(Span::call_site(), "program extension missing name"))?,
            intent: intent.ok_or_else(|| syn::Error::new(Span::call_site(), "program extension missing intent"))?,
            async_run,
            sampled,
            apis,
        })
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
    let mut tag = Tag::Ordinary;
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
                tag = parsed.0;
                extensions = parsed.1;
            }
            other => return Err(syn::Error::new_spanned(&key, format!("unknown classified field `{other}`"))),
        }
    }
    let ty = ty.ok_or_else(|| syn::Error::new(Span::call_site(), "actor envelope missing ty"))?;
    Ok(Envelope { ty, namespace, extensions, tag })
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

fn parse_extensions(input: ParseStream<'_>) -> syn::Result<(Tag, TokenStream2)> {
    let mut tag = Tag::Ordinary;
    let mut tokens = TokenStream2::new();
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        let payload;
        braced!(payload in input);
        let mut payload_tokens = TokenStream2::new();
        while !payload.is_empty() {
            let tt: TokenTree = payload.parse()?;
            payload_tokens.extend([tt]);
        }
        if key == PROGRAM_EXTENSION {
            if matches!(tag, Tag::Reactor) {
                return Err(syn::Error::new_spanned(
                    &key,
                    "a type cannot carry both aether_bloomery_program and aether_bloomery_reactor extensions",
                ));
            }
            tag = Tag::Program(Box::new(syn::parse2(payload_tokens.clone())?));
        } else if key == REACTOR_EXTENSION {
            if matches!(tag, Tag::Program(_)) {
                return Err(syn::Error::new_spanned(
                    &key,
                    "a type cannot carry both aether_bloomery_program and aether_bloomery_reactor extensions",
                ));
            }
            tag = Tag::Reactor;
        }
        tokens.extend(quote! { #key { #payload_tokens } });
    }
    Ok((tag, tokens))
}
