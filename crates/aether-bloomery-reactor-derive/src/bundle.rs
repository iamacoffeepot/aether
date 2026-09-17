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
    let live_fn = format_ident!("__aether_{views}_fold_live");
    let warmup_fn = format_ident!("__aether_{views}_fold_warmup");
    let emit_fn = format_ident!("__aether_{views}_emit_outputs");
    let ack_prepared_fn = format_ident!("__aether_{views}_ack_prepared");
    let ack_evaluated_fn = format_ident!("__aether_{views}_ack_evaluated");
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
    let live = live_fn_tokens(&live_fn, peers);
    let warmup = warmup_fn_tokens(&warmup_fn, peers);
    let emit = emit_outputs_fn(&emit_fn);
    let ack = ack_fn_tokens(&ack_prepared_fn, &ack_evaluated_fn);
    let event = event_handler_tokens(&live_fn, &ack_prepared_fn, &ack_evaluated_fn, peers);
    let batch = batch_handler_tokens(&warmup_fn, &ack_prepared_fn);
    quote! {
        struct #views {
            cluster: ::aether_bloomery_reactor::Cluster,
            output: ::aether_actor::__macro_internals::String,
            ack: ::aether_actor::__macro_internals::String,
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
                    cluster: ::aether_bloomery_reactor::Cluster::new(),
                    output: config.output,
                    ack: config.ack,
                })
            }

            fn wire(&mut self, ctx: &mut ::aether_actor::WireCtx<'_, '_>) {
                let config = ::aether_bloomery_reactor::ClusterConfig {
                    output: self.output.clone(),
                    ack: self.ack.clone(),
                };
                #(#spawn_peers)*
            }

            #event

            #batch

            #[handler::single]
            fn on_peer_evaluated(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_>,
                outcome: ::aether_bloomery_reactor::PeerEvaluated,
            ) {
                let Some(source) = ctx.source_mailbox() else {
                    return;
                };
                if let Some(evaluated) = self.cluster.note_peer(source, &outcome) {
                    #ack_evaluated_fn(ctx, &self.ack, &evaluated);
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
                    ctx.reply(&self.cluster.status());
                }
            }
        }

        #live
        #warmup
        #ack
        #emit
        #(#peer_structs)*
    }
}

fn event_handler_tokens(
    live_fn: &Ident,
    ack_prepared_fn: &Ident,
    ack_evaluated_fn: &Ident,
    peers: &[ReactorPeer],
) -> TokenStream2 {
    let delivery = peer_delivery_tokens(peers, ack_evaluated_fn);
    quote! {
        #[handler::manual]
        fn on_event(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
            event: ::aether_bloomery_reactor::Event,
        ) {
            use ::aether_actor::OutboundReply;
            let before = self.cluster.owner().cursor();
            if let Err(error) = self.cluster.check_admit(&event.stream) {
                let result = ::aether_bloomery_reactor::PreparedResult::from_error(
                    event.stream,
                    before.0,
                    &error,
                );
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
            let stream = event.stream;
            match #live_fn(self.cluster.owner_mut(), stream.clone(), event.entry) {
                Ok(prepared) => {
                    self.cluster.bind_stream(stream.clone());
                    self.cluster.trust_cursor();
                    let result = ::aether_bloomery_reactor::PreparedResult::ok(
                        stream.clone(),
                        prepared.seq,
                    );
                    #ack_prepared_fn(ctx, &self.ack, &result);
                    if ctx.reply_target().is_some() {
                        ctx.reply(&result);
                    }
                    #delivery
                    if let Some(evaluated) = self.cluster.start_live(stream, prepared.seq, &expected) {
                        #ack_evaluated_fn(ctx, &self.ack, &evaluated);
                    }
                }
                Err(error) => {
                    if self.cluster.owner().cursor() != before || self.cluster.owner().is_poisoned() {
                        self.cluster.mark_poisoned();
                        self.cluster.bind_stream(stream.clone());
                    }
                    let result = ::aether_bloomery_reactor::PreparedResult::from_error(
                        stream,
                        self.cluster.owner().cursor().0,
                        &error,
                    );
                    #ack_prepared_fn(ctx, &self.ack, &result);
                    if ctx.reply_target().is_some() {
                        ctx.reply(&result);
                    }
                }
            }
        }
    }
}

fn batch_handler_tokens(warmup_fn: &Ident, ack_prepared_fn: &Ident) -> TokenStream2 {
    quote! {
        #[handler::manual]
        fn on_batch(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
            batch: ::aether_bloomery_reactor::EventBatch,
        ) {
            use ::aether_actor::OutboundReply;
            let before = self.cluster.owner().cursor();
            if let Err(error) = self.cluster.check_admit(&batch.stream) {
                let result = ::aether_bloomery_reactor::PreparedResult::from_error(
                    batch.stream,
                    before.0,
                    &error,
                );
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
            if let Err(error) = batch.validate() {
                let result = ::aether_bloomery_reactor::PreparedResult::from_error(
                    batch.stream,
                    before.0,
                    &error,
                );
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
            let mut fold = Ok(());
            for item in batch.entries {
                if let Err(error) = #warmup_fn(self.cluster.owner_mut(), item) {
                    fold = Err(error);
                    break;
                }
            }
            match fold {
                Ok(()) => {
                    self.cluster.bind_stream(batch.stream.clone());
                    self.cluster.trust_cursor();
                    let result = ::aether_bloomery_reactor::PreparedResult::ok(
                        batch.stream,
                        self.cluster.owner().cursor().0,
                    );
                    #ack_prepared_fn(ctx, &self.ack, &result);
                    if ctx.reply_target().is_some() {
                        ctx.reply(&result);
                    }
                }
                Err(error) => {
                    if self.cluster.owner().cursor() != before || self.cluster.owner().is_poisoned() {
                        self.cluster.mark_poisoned();
                        self.cluster.bind_stream(batch.stream.clone());
                    }
                    let result = ::aether_bloomery_reactor::PreparedResult::from_error(
                        batch.stream,
                        self.cluster.owner().cursor().0,
                        &error,
                    );
                    #ack_prepared_fn(ctx, &self.ack, &result);
                    if ctx.reply_target().is_some() {
                        ctx.reply(&result);
                    }
                }
            }
        }
    }
}

fn peer_delivery_tokens(peers: &[ReactorPeer], ack_evaluated_fn: &Ident) -> TokenStream2 {
    let peer_count = peers.len();
    let peer_lookups = peers.iter().enumerate().map(|(index, peer)| {
        let ty = &peer.peer;
        let subname = &peer.subname;
        let ident = format_ident!("__aether_peer_{index}");
        quote! {
            let #ident = ctx.child_as::<#ty>(#subname);
        }
    });
    let send_peers = peers.iter().enumerate().map(|(index, _peer)| {
        let ident = format_ident!("__aether_peer_{index}");
        quote! {
            if let Some(peer) = #ident {
                expected.push(peer.id());
                peer.send(ctx, &prepared);
            }
        }
    });
    quote! {
        #(#peer_lookups)*
        let mut expected = ::aether_bloomery_reactor::__macro_internals::Vec::new();
        #(#send_peers)*
        if expected.len() != #peer_count {
            let evaluated = ::aether_bloomery_reactor::EvaluatedResult::from_error(
                stream,
                prepared.seq,
                "reactor peer is missing",
            );
            #ack_evaluated_fn(ctx, &self.ack, &evaluated);
            return;
        }
    }
}

fn warm_tokens(peers: &[ReactorPeer]) -> impl Iterator<Item = TokenStream2> + '_ {
    peers.iter().map(|peer| {
        let reactor = &peer.reactor;
        quote! { ::aether_bloomery_reactor::warm_reactor::<#reactor>(owner)?; }
    })
}

fn snapshot_tokens(peers: &[ReactorPeer]) -> impl Iterator<Item = TokenStream2> + '_ {
    peers.iter().map(|peer| {
        let reactor = &peer.reactor;
        quote! {
            ::aether_bloomery_reactor::extend_snapshots(
                &mut views,
                ::aether_bloomery_reactor::snapshot_reactor::<#reactor>(owner)?,
            )?;
        }
    })
}

fn live_fn_tokens(live_fn: &Ident, peers: &[ReactorPeer]) -> TokenStream2 {
    let preceding = warm_tokens(peers);
    let through_n = warm_tokens(peers);
    let snapshot_reactors = snapshot_tokens(peers);
    quote! {
        fn #live_fn(
            owner: &mut ::aether_bloomery_reactor::Owner,
            stream: ::aether_actor::__macro_internals::String,
            item: ::aether_bloomery_reactor::JournalEntry,
        ) -> Result<::aether_bloomery_reactor::PreparedPrefix, ::aether_bloomery_reactor::PrepareError> {
            #(#preceding)*
            let entry = item.to_entry();
            owner.push(::core::slice::from_ref(&entry))?;
            #(#through_n)*
            let mut views = ::aether_bloomery_reactor::__macro_internals::Vec::new();
            #(#snapshot_reactors)*
            Ok(::aether_bloomery_reactor::PreparedPrefix::from_parts(stream, &entry, views))
        }
    }
}

fn warmup_fn_tokens(warmup_fn: &Ident, peers: &[ReactorPeer]) -> TokenStream2 {
    let warm_reactors = warm_tokens(peers);
    quote! {
        fn #warmup_fn(
            owner: &mut ::aether_bloomery_reactor::Owner,
            item: ::aether_bloomery_reactor::JournalEntry,
        ) -> Result<(), ::aether_bloomery_reactor::PrepareError> {
            let entry = item.to_entry();
            owner.push(::core::slice::from_ref(&entry))?;
            #(#warm_reactors)*
            Ok(())
        }
    }
}

fn ack_fn_tokens(ack_prepared_fn: &Ident, ack_evaluated_fn: &Ident) -> TokenStream2 {
    quote! {
        fn #ack_prepared_fn<M: ::aether_actor::ReplyMode>(
            ctx: &mut ::aether_actor::WasmCtx<'_, M>,
            ack: &str,
            payload: &::aether_bloomery_reactor::PreparedResult,
        ) {
            use ::aether_actor::MailSender;
            if !ack.is_empty() {
                ctx.send_to_named(ack, payload);
            }
        }

        fn #ack_evaluated_fn<M: ::aether_actor::ReplyMode>(
            ctx: &mut ::aether_actor::WasmCtx<'_, M>,
            ack: &str,
            payload: &::aether_bloomery_reactor::EvaluatedResult,
        ) {
            use ::aether_actor::MailSender;
            if !ack.is_empty() {
                ctx.send_to_named(ack, payload);
            }
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
                let stream = prepared.stream.clone();
                let seq = prepared.seq;
                let outcome = match prepared.into_owner::<#reactor>() {
                    Err(error) => ::aether_bloomery_reactor::PeerEvaluated::from_error(stream, seq, &error),
                    Ok(mut owner) => match <#reactor as ::aether_bloomery_reactor::Reactor>::evaluate(&self.reactor, &mut owner) {
                        Err(error) => ::aether_bloomery_reactor::PeerEvaluated::from_error(stream, seq, &error),
                        Ok(intents) => {
                            #emit_fn::<#reactor>(ctx, &self.output, &intents);
                            ::aether_bloomery_reactor::PeerEvaluated::ok(stream, seq)
                        }
                    },
                };
                if let Some(parent) = ctx.source_mailbox() {
                    ctx.send_to(parent, &outcome);
                }
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
            <R as ::aether_bloomery_reactor::Reactor>::visit_arms(&mut emit);
        }
    }
}
