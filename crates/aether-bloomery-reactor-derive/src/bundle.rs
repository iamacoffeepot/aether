//! `ReactorBundle` export generator: select bloomery reactor extensions from
//! the framework-owned descriptor list, emit one shared views coordinator plus
//! inline peers, then continue the `export!` generator pipeline.

use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, LitStr, Path, Token, Type, braced, bracketed};

// Keep in lockstep with `aether_bloomery_reactor::CLUSTER_NAMESPACE`. The derive
// crate cannot read that const (runtime → derive dependency), so reserved-namespace
// detection compares against this copy.
const CLUSTER_NAMESPACE: &str = "aether.bloomery.reactor";
const COORDINATOR_IDENT: &str = "__AetherBloomeryReactorCluster";
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
            return Err(syn::Error::new(Span::call_site(), "ReactorBundle requires at least one export type"));
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
            && namespace == CLUSTER_NAMESPACE
        {
            return Err(syn::Error::new_spanned(
                &entry.ty,
                format!("NAMESPACE `{namespace}` is reserved for the generated views coordinator"),
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
            "ReactorBundle found no #[reactor] exports in this export! set",
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
                "export! default cannot be a reactor; a reactor-only module exports the views coordinator"
            },
        ));
    }

    let mut peers = Vec::new();
    for entry in &reactors {
        let NamespaceTok::Lit(namespace) = &entry.namespace else {
            unreachable!("reactor namespace checked above");
        };
        let hash = fnv1a_64(namespace.as_bytes());
        peers.push(ReactorPeer {
            reactor: entry.ty.clone(),
            peer: format_ident!("__AetherBloomeryReactorPeer_n{:x}", hash),
            subname: format!("r_{hash:x}"),
        });
    }

    let coordinator = format_ident!("{COORDINATOR_IDENT}");
    let cluster = expand_cluster(&coordinator, &peers);
    let boot_tokens = optional_type_tokens(boot.as_ref());
    let default_tokens = optional_type_tokens(default.as_ref());
    let actor_tokens = actors.iter().map(envelope_tokens);
    let export_tokens = rewritten_exports(&exports, &reactors, &coordinator);
    let rest = remaining_generators.iter();
    Ok(quote! {
        #cluster
        ::aether_actor::__export_continue! {
            remaining_generators: [ #(#rest),* ]
            boot: #boot_tokens
            default: #default_tokens
            actors: [
                #(#actor_tokens)*
                { ty: { #coordinator } namespace: #CLUSTER_NAMESPACE extensions: [] }
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

struct ReactorPeer {
    reactor: Type,
    peer: Ident,
    subname: String,
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

fn expand_cluster(views: &Ident, peers: &[ReactorPeer]) -> TokenStream2 {
    let push_fn = format_ident!("__aether_{views}_push_and_prepare");
    let emit_fn = format_ident!("__aether_{views}_emit_outputs");
    let peer_structs = peers.iter().map(|peer| expand_peer(views, peer, &emit_fn));
    let spawn_peers = peers.iter().map(|peer| {
        let ty = &peer.peer;
        let subname = &peer.subname;
        quote! {
            let _ = ctx.spawn_inline_child::<#views, #ty>(
                ::aether_actor::Subname::Named(#subname),
                &config,
            );
        }
    });
    let send_peers = peers.iter().map(|peer| {
        let ty = &peer.peer;
        let subname = &peer.subname;
        quote! {
            if let Some(peer) = ctx.child_as::<#ty>(#subname) {
                peer.send(ctx, &prepared);
            }
        }
    });
    let prepare = preparation_fn(&push_fn, peers);
    let emit = emit_outputs_fn(&emit_fn);
    quote! {
        struct #views {
            owner: ::aether_bloomery_reactor::Owner,
            output: ::aether_actor::__macro_internals::String,
        }

        #[::aether_actor::actor]
        impl ::aether_actor::WasmActor for #views {
            type Config = ::aether_bloomery_reactor::ClusterConfig;
            const NAMESPACE: &'static str = ::aether_bloomery_reactor::CLUSTER_NAMESPACE;

            fn init(
                config: ::aether_bloomery_reactor::ClusterConfig,
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                Ok(Self {
                    owner: ::aether_bloomery_reactor::Owner::new(),
                    output: config.output,
                })
            }

            fn wire(&mut self, ctx: &mut ::aether_actor::WireCtx<'_, '_>) {
                let config = ::aether_bloomery_reactor::ClusterConfig { output: self.output.clone() };
                #(#spawn_peers)*
            }

            #[handler::manual]
            fn on_push(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                push: ::aether_bloomery_reactor::PushEntries,
            ) {
                use ::aether_actor::OutboundReply;
                let mut result = Ok(self.owner.cursor().0);
                for item in push.entries {
                    match #push_fn(&mut self.owner, item) {
                        Ok(prepared) => {
                            #(#send_peers)*
                            result = Ok(self.owner.cursor().0);
                        }
                        Err(error) => {
                            result = Err(error);
                            break;
                        }
                    }
                }
                if ctx.reply_target().is_some() {
                    ctx.reply(&::aether_bloomery_reactor::PushResult::from_prepare(result));
                }
            }

            #[handler::manual]
            fn on_status(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
                _query: ::aether_bloomery_reactor::ClusterStatusQuery,
            ) {
                use ::aether_actor::OutboundReply;
                if ctx.reply_target().is_some() {
                    ctx.reply(&::aether_bloomery_reactor::ClusterStatus { cursor: self.owner.cursor().0 });
                }
            }
        }

        #prepare
        #emit
        #(#peer_structs)*
    }
}

fn preparation_fn(push_fn: &Ident, peers: &[ReactorPeer]) -> TokenStream2 {
    let warm_reactors = peers.iter().map(|peer| {
        let reactor = &peer.reactor;
        quote! { ::aether_bloomery_reactor::warm_reactor::<#reactor>(owner)?; }
    });
    let snapshot_reactors = peers.iter().map(|peer| {
        let reactor = &peer.reactor;
        quote! {
            ::aether_bloomery_reactor::extend_snapshots(
                &mut views,
                ::aether_bloomery_reactor::snapshot_reactor::<#reactor>(owner)?,
            )?;
        }
    });
    quote! {
        fn #push_fn(
            owner: &mut ::aether_bloomery_reactor::Owner,
            item: ::aether_bloomery_reactor::JournalEntry,
        ) -> Result<::aether_bloomery_reactor::PreparedPrefix, ::aether_bloomery_reactor::PrepareError> {
            let entry = item.to_entry();
            owner.push(::core::slice::from_ref(&entry))?;
            #(#warm_reactors)*
            let mut views = ::aether_bloomery_reactor::__macro_internals::Vec::new();
            #(#snapshot_reactors)*
            Ok(::aether_bloomery_reactor::PreparedPrefix::from_parts(&entry, views))
        }
    }
}

fn expand_peer(views: &Ident, peer: &ReactorPeer, emit_fn: &Ident) -> TokenStream2 {
    let ReactorPeer { reactor, peer: peer_ty, .. } = peer;
    quote! {
        struct #peer_ty {
            reactor: #reactor,
            output: ::aether_actor::__macro_internals::String,
        }

        #[::aether_actor::actor(instanced, child_of(#views))]
        impl ::aether_actor::WasmActor for #peer_ty {
            type Config = ::aether_bloomery_reactor::ClusterConfig;
            const NAMESPACE: &'static str = <#reactor as ::aether_bloomery_reactor::Reactor>::NAMESPACE;

            fn init(
                config: ::aether_bloomery_reactor::ClusterConfig,
                _ctx: &mut ::aether_actor::WasmInitCtx<'_>,
            ) -> Result<Self, ::aether_actor::ActorInitError> {
                Ok(Self { reactor: #reactor, output: config.output })
            }

            #[handler::single]
            fn on_prepared(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_>,
                prepared: ::aether_bloomery_reactor::PreparedPrefix,
            ) {
                let Ok(mut owner) = prepared.into_owner::<#reactor>() else {
                    return;
                };
                let Ok(intents) = <#reactor as ::aether_bloomery_reactor::Reactor>::evaluate(&self.reactor, &mut owner) else {
                    return;
                };
                #emit_fn::<#reactor>(ctx, &self.output, &intents);
            }
        }
    }
}

fn emit_outputs_fn(emit_fn: &Ident) -> TokenStream2 {
    quote! {
        fn #emit_fn<R: ::aether_bloomery_reactor::Reactor>(
            ctx: &mut ::aether_actor::WasmCtx<'_>,
            output: &str,
            intents: &[::aether_bloomery_reactor::Intent],
        ) {
            use ::aether_actor::MailSender;
            if output.is_empty() {
                return;
            }
            struct Emit<'c, 'm, 'o, 'i, M: ::aether_actor::ReplyMode> {
                ctx: &'c mut ::aether_actor::WasmCtx<'m, M>,
                output: &'o str,
                intents: &'i [::aether_bloomery_reactor::Intent],
                seen: ::aether_bloomery_reactor::__macro_internals::BTreeSet<&'static str>,
            }
            impl<M: ::aether_actor::ReplyMode> ::aether_bloomery_reactor::ArmVisitor for Emit<'_, '_, '_, '_, M> {
                fn visit<T, L, O>(&mut self, _name: &'static str)
                where
                    T: ::aether_bloomery_reactor::Trigger,
                    L: ::aether_bloomery_reactor::Params<T>,
                    L::Views: ::aether_bloomery_reactor::PublishSet,
                    O: ::aether_bloomery_reactor::Output,
                {
                    if !self.seen.insert(O::NAME) {
                        return;
                    }
                    for intent in self.intents {
                        if let Some(value) = intent.decode::<O>() {
                            self.ctx.send_to_named(self.output, &value);
                        }
                    }
                }
            }
            let mut emit = Emit {
                ctx,
                output,
                intents,
                seen: ::aether_bloomery_reactor::__macro_internals::BTreeSet::new(),
            };
            R::visit_arms(&mut emit);
        }
    }
}
