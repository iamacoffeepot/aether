//! `bundle_programs` export generator: select bloomery program extensions from
//! the framework-owned descriptor list, emit one bundle root plus an inline
//! invocation child, then continue the `export!` generator pipeline.

use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, LitStr, Path, Token, Type, braced, bracketed};

// Keep in lockstep with `aether_bloomery_program::PROGRAM_NAMESPACE`. The derive
// crate cannot read that const (runtime → derive dependency), so reserved-namespace
// detection compares against this copy.
const PROGRAM_NAMESPACE: &str = "aether.bloomery.program";
const ROOT_IDENT: &str = "__AetherBloomeryProgramRoot";
const INVOCATION_IDENT: &str = "__AetherBloomeryProgramInvocation";
const TABLE_IDENT: &str = "__AETHER_BLOOMERY_PROGRAM_TABLE";
const PROGRAM_EXTENSION: &str = "aether_bloomery_program";
const INVOCATION_NAMESPACE: &str = "aether.bloomery.program.invocation";

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
    program: Option<ProgramMeta>,
}

#[derive(Clone)]
struct ProgramMeta {
    name: LitStr,
    intent: LitStr,
    input: Type,
    result: Type,
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
            return Err(syn::Error::new(Span::call_site(), "bundle_programs requires at least one export type"));
        }
        Ok(Self { remaining_generators, boot, default, actors, exports })
    }
}

impl Parse for ProgramMeta {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut intent = None;
        let mut input_ty = None;
        let mut result = None;
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
                    if mode != "Pure" {
                        return Err(syn::Error::new_spanned(mode, "bundle_programs requires Mode::Pure"));
                    }
                }
                "input" => input_ty = Some(input.parse()?),
                "result" => result = Some(input.parse()?),
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
            input: input_ty.ok_or_else(|| syn::Error::new(Span::call_site(), "program extension missing input"))?,
            result: result.ok_or_else(|| syn::Error::new(Span::call_site(), "program extension missing result"))?,
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
    let mut program = None;
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
                program = parsed.0;
                extensions = parsed.1;
            }
            other => return Err(syn::Error::new_spanned(&key, format!("unknown classified field `{other}`"))),
        }
    }
    let ty = ty.ok_or_else(|| syn::Error::new(Span::call_site(), "actor envelope missing ty"))?;
    Ok(Envelope { ty, namespace, extensions, program })
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

fn parse_extensions(input: ParseStream<'_>) -> syn::Result<(Option<ProgramMeta>, TokenStream2)> {
    let mut program = None;
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
            program = Some(syn::parse2(payload_tokens.clone())?);
        }
        tokens.extend(quote! { #key { #payload_tokens } });
    }
    Ok((program, tokens))
}

fn expand_generate(input: GenerateInput) -> syn::Result<TokenStream2> {
    let GenerateInput { remaining_generators, boot, default, actors, exports } = input;
    for entry in &actors {
        if let NamespaceTok::Lit(namespace) = &entry.namespace
            && namespace == PROGRAM_NAMESPACE
        {
            return Err(syn::Error::new_spanned(
                &entry.ty,
                format!("NAMESPACE `{namespace}` is reserved for the generated program bundle root"),
            ));
        }
    }

    let mut programs = Vec::new();
    let mut seen_names = Vec::new();
    for export_ty in &exports {
        let Some(entry) = actors.iter().find(|entry| types_eq(export_ty, &entry.ty)) else {
            continue;
        };
        let Some(meta) = &entry.program else {
            continue;
        };
        let name = meta.name.value();
        if seen_names.iter().any(|existing| existing == &name) {
            return Err(syn::Error::new_spanned(&entry.ty, format!("duplicate program NAME `{name}`")));
        }
        seen_names.push(name);
        programs.push(ProgramEntry { ty: entry.ty.clone(), meta: meta.clone() });
    }
    if programs.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "bundle_programs found no #[program] exports in this export! set",
        ));
    }
    if let Some(boot) = &boot
        && type_in(boot, programs.iter().map(|entry| &entry.ty))
    {
        return Err(syn::Error::new_spanned(boot, "export! boot type cannot be a program"));
    }
    if let Some(default) = &default
        && type_in(default, programs.iter().map(|entry| &entry.ty))
    {
        return Err(syn::Error::new_spanned(default, "export! default cannot be a program"));
    }

    let root = format_ident!("{ROOT_IDENT}");
    let invocation = format_ident!("{INVOCATION_IDENT}");
    let bundle = expand_bundle(&root, &invocation, &programs);
    let boot_tokens = optional_type_tokens(boot.as_ref());
    // A program-only rewrite is one type. `export!(Root)` is the single-actor
    // form, which writes no ActorBoundary, so `export: Some(PROGRAM_NAMESPACE)`
    // cannot resolve. Naming the generated root as default uses the multi-actor
    // `export!(default = Root)` arm, which emits the boundary the host matches.
    let default_tokens = default.as_ref().map_or_else(|| quote! { { #root } }, |ty| quote! { { #ty } });
    let actor_tokens = actors.iter().map(envelope_tokens);
    let export_tokens = rewritten_exports(&exports, &programs, &root);
    let rest = remaining_generators.iter();
    Ok(quote! {
        #bundle
        ::aether_actor::__export_continue! {
            remaining_generators: [ #(#rest),* ]
            boot: #boot_tokens
            default: #default_tokens
            actors: [
                #(#actor_tokens)*
                { ty: { #root } namespace: #PROGRAM_NAMESPACE extensions: [] }
            ]
            exports: [ #export_tokens ]
        }
    })
}

struct ProgramEntry {
    ty: Type,
    meta: ProgramMeta,
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

fn rewritten_exports(exports: &[Type], programs: &[ProgramEntry], root: &Ident) -> TokenStream2 {
    let mut inserted = false;
    let mut out = TokenStream2::new();
    for ty in exports {
        if type_in(ty, programs.iter().map(|entry| &entry.ty)) {
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

fn optional_type_tokens(ty: Option<&Type>) -> TokenStream2 {
    ty.map_or_else(|| quote! { none }, |ty| quote! { { #ty } })
}

fn type_in<'a>(needle: &Type, haystack: impl IntoIterator<Item = &'a Type>) -> bool {
    haystack.into_iter().any(|ty| types_eq(needle, ty))
}

fn types_eq(left: &Type, right: &Type) -> bool {
    quote!(#left).to_string() == quote!(#right).to_string()
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn expand_bundle(root: &Ident, invocation: &Ident, programs: &[ProgramEntry]) -> TokenStream2 {
    let table = format_ident!("{TABLE_IDENT}");
    let table_static = expand_table(&table, programs);
    let root_actor = expand_root(root, invocation, &table);
    let invocation_actor = expand_invocation(root, invocation, &table);
    let sections = programs.iter().map(expand_section);
    quote! {
        #table_static
        #root_actor
        #invocation_actor
        #(#sections)*
    }
}

fn expand_table(table: &Ident, programs: &[ProgramEntry]) -> TokenStream2 {
    let entries = programs.iter().map(|program| {
        let ty = &program.ty;
        quote! { ::aether_bloomery_program::ProgramEntry::of::<#ty>() }
    });
    quote! {
        static #table: ::aether_bloomery_program::ProgramTable =
            ::aether_bloomery_program::__macro_internals::program_table(&[#(#entries),*]);
    }
}

fn expand_root(root: &Ident, invocation: &Ident, table: &Ident) -> TokenStream2 {
    quote! {
        struct #root {
            inner: ::aether_bloomery_program::Root<::core::option::Option<::aether_actor::ReplyHandle>>,
        }

        #[::aether_actor::actor]
        impl ::aether_actor::WasmActor for #root {
            const NAMESPACE: &'static str = ::aether_bloomery_program::PROGRAM_NAMESPACE;

            fn init(
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                Ok(Self { inner: ::aether_bloomery_program::Root::new(&#table) })
            }

            #[handler::manual]
            fn on_invoke(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                invoke: ::aether_bloomery_program::Invoke,
            ) {
                use ::aether_actor::OutboundReply;
                match self.inner.admit(&invoke) {
                    ::core::result::Result::Err(rejected) => {
                        if ctx.reply_target().is_some() {
                            ctx.reply(&rejected);
                        }
                    }
                    ::core::result::Result::Ok(admission) => {
                        let seq = admission.seq();
                        let seq_name =
                            ::aether_bloomery_program::__macro_internals::ToString::to_string(&seq);
                        match ctx.spawn_inline_child::<#root, #invocation>(
                            ::aether_actor::Subname::Named(&seq_name),
                            &(),
                        ) {
                            ::core::result::Result::Ok(child) => {
                                admission.start(child.id(), ctx.reply_target());
                                child.send(ctx, &invoke);
                            }
                            ::core::result::Result::Err(_) => {
                                let rejected = admission.spawn_failed();
                                if ctx.reply_target().is_some() {
                                    ctx.reply(&rejected);
                                }
                            }
                        }
                    }
                }
            }

            #[handler::manual]
            fn on_invoked(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                invoked: ::aether_bloomery_program::Invoked,
            ) {
                use ::aether_actor::OutboundReply;
                let Some((child, reply)) = self.inner.finish(&invoked, ctx.source_mailbox()) else {
                    return;
                };
                if let Some(reply) = reply {
                    ctx.reply_to(reply, &invoked);
                }
                ctx.despawn_inline_child(child);
            }
        }
    }
}

fn expand_invocation(root: &Ident, invocation: &Ident, table: &Ident) -> TokenStream2 {
    quote! {
        struct #invocation;

        #[::aether_actor::actor(instanced, child_of(#root))]
        impl ::aether_actor::WasmActor for #invocation {
            const NAMESPACE: &'static str = #INVOCATION_NAMESPACE;

            fn init(
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                Ok(Self)
            }

            #[handler::single]
            fn on_invoke(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_>,
                invoke: ::aether_bloomery_program::Invoke,
            ) {
                let _ = self;
                let invoked = ::aether_bloomery_program::dispatch(&#table, invoke);
                if let Some(parent) = ctx.source_mailbox() {
                    ctx.send_to(parent, &invoked);
                }
            }
        }
    }
}

fn expand_section(program: &ProgramEntry) -> TokenStream2 {
    let name = &program.meta.name;
    let intent = &program.meta.intent;
    let input = &program.meta.input;
    let result = &program.meta.result;
    let hash = fnv1a_64(name.value().as_bytes());
    let len_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_LEN_{hash:016X}");
    let bytes_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_BYTES_{hash:016X}");
    let section_ident = format_ident!("__AETHER_BLOOMERY_PROGRAM_SECTION_{hash:016X}");
    quote! {
        const #len_ident: usize = ::aether_bloomery_program::__macro_internals::program_record_len(
            #name.as_bytes(),
            #intent.as_bytes(),
        );
        const #bytes_ident: [u8; #len_ident] = ::aether_bloomery_program::__macro_internals::write_program_record::<#len_ident>(
            #name.as_bytes(),
            <#input as ::aether_bloomery_program::__macro_internals::Kind>::ID.0,
            <#result as ::aether_bloomery_program::__macro_internals::Kind>::ID.0,
            ::aether_bloomery_program::__macro_internals::MODE_PURE,
            #intent.as_bytes(),
        );
        const _: &[u8] = &#bytes_ident;
        #[cfg(target_family = "wasm")]
        #[unsafe(link_section = "aether.bloomery.programs")]
        static #section_ident: [u8; #len_ident] = #bytes_ident;
    }
}
