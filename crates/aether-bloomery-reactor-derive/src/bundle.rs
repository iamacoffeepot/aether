//! `bundle_reactors` export generator: select bloomery reactor extensions from
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
            namespace: namespace.clone(),
            subname: format!("r_{hash:x}"),
        });
    }

    let coordinator = format_ident!("{COORDINATOR_IDENT}");
    let cluster = expand_cluster(&coordinator, &peers);
    let boot_tokens = optional_type_tokens(boot.as_ref());
    let default_tokens = if default.is_none() && !mixed {
        quote! { { #coordinator } }
    } else {
        optional_type_tokens(default.as_ref())
    };
    let actor_tokens = actors.iter().map(envelope_tokens);
    let peer_actor_tokens = peers.iter().map(|peer| {
        let ty = &peer.peer;
        let namespace = &peer.namespace;
        quote! { { ty: { #ty } namespace: #namespace extensions: [] } }
    });
    let export_tokens = rewritten_exports(&exports, &reactors, &coordinator, &peers);
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
                #(#peer_actor_tokens)*
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

/// Replace selected reactor exports with the coordinator and public peers.
fn rewritten_exports(
    exports: &[Type],
    reactors: &[&Envelope],
    coordinator: &Ident,
    peers: &[ReactorPeer],
) -> TokenStream2 {
    let mut inserted = false;
    let mut out = TokenStream2::new();
    for ty in exports {
        if type_in(ty, reactors.iter().map(|entry| &entry.ty)) {
            if !inserted {
                out.extend(quote! { { #coordinator } });
                for peer in peers {
                    let peer_ty = &peer.peer;
                    out.extend(quote! { { #peer_ty } });
                }
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
    namespace: String,
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

struct ManagedNames<'a> {
    warmup: &'a Ident,
    live: &'a Ident,
    ack_prepared: &'a Ident,
    ack_evaluated: &'a Ident,
    drain: &'a Ident,
    request: &'a Ident,
    fail: &'a Ident,
}

fn expand_cluster(views: &Ident, peers: &[ReactorPeer]) -> TokenStream2 {
    let live_fn = format_ident!("__aether_{views}_fold_live");
    let warmup_fn = format_ident!("__aether_{views}_fold_warmup");
    let emit_fn = format_ident!("__aether_{views}_emit_outputs");
    let ack_prepared_fn = format_ident!("__aether_{views}_ack_prepared");
    let ack_evaluated_fn = format_ident!("__aether_{views}_ack_evaluated");
    let drain_fn = format_ident!("__aether_{views}_drain_live");
    let request_fn = format_ident!("__aether_{views}_request_history");
    let fail_fn = format_ident!("__aether_{views}_fail_managed");
    let peer_structs = peers.iter().map(|peer| expand_peer(views, peer, &emit_fn));
    let spawn_peers = spawn_peer_tokens(views, peers);
    let live = live_fn_tokens(&live_fn, peers);
    let warmup = warmup_fn_tokens(&warmup_fn, peers);
    let emit = emit_outputs_fn(&emit_fn);
    let ack = ack_fn_tokens(&ack_prepared_fn, &ack_evaluated_fn);
    let event = event_handler_tokens(&live_fn, &ack_prepared_fn, &ack_evaluated_fn, &drain_fn, peers);
    let batch = batch_handler_tokens(&warmup_fn, &ack_prepared_fn);
    let managed_names = ManagedNames {
        warmup: &warmup_fn,
        live: &live_fn,
        ack_prepared: &ack_prepared_fn,
        ack_evaluated: &ack_evaluated_fn,
        drain: &drain_fn,
        request: &request_fn,
        fail: &fail_fn,
    };
    let (managed_handlers, managed_helpers) = managed_handler_tokens(views, &managed_names, peers);
    quote! {
        struct #views {
            cluster: ::aether_bloomery_reactor::Cluster,
            feed: ::aether_bloomery_reactor::ManagedFeed,
            output: ::aether_actor::__macro_internals::String,
            ack: ::aether_actor::__macro_internals::String,
            peer_birth_error: Option<::aether_actor::SpawnError>,
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
                    feed: ::aether_bloomery_reactor::ManagedFeed::new(),
                    output: config.output,
                    ack: config.ack,
                    peer_birth_error: None,
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

            #managed_handlers

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
        #managed_helpers
        #ack
        #emit
        #(#peer_structs)*
    }
}

fn spawn_peer_tokens(views: &Ident, peers: &[ReactorPeer]) -> Vec<TokenStream2> {
    peers
        .iter()
        .map(|peer| {
            let ty = &peer.peer;
            let subname = &peer.subname;
            quote! {
                if let Err(error) = ctx.spawn_inline_child::<#views, #ty>(
                    ::aether_actor::Subname::Named(#subname),
                    &config,
                ) && self.peer_birth_error.is_none() {
                    self.peer_birth_error = Some(error);
                }
            }
        })
        .collect()
}

fn event_handler_tokens(
    live_fn: &Ident,
    ack_prepared_fn: &Ident,
    ack_evaluated_fn: &Ident,
    drain_fn: &Ident,
    peers: &[ReactorPeer],
) -> TokenStream2 {
    let delivery = peer_delivery_tokens(peers, ack_evaluated_fn, &quote! { return; });
    quote! {
        #[handler::manual]
        fn on_event(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
            event: ::aether_bloomery_reactor::Event,
        ) {
            use ::aether_actor::OutboundReply;
            let before = self.cluster.owner().cursor();
            if matches!(
                &self.feed.mode,
                ::aether_bloomery_reactor::FeedMode::Warming(_)
                    | ::aether_bloomery_reactor::FeedMode::Feeding { .. }
            ) {
                if let Err(reason) = self.feed.push_live(&event.stream, event.entry) {
                    ctx.fatal_abort(reason.into());
                }
                self.#drain_fn(ctx);
                return;
            }
            if matches!(&self.feed.mode, ::aether_bloomery_reactor::FeedMode::Failed) {
                let result = ::aether_bloomery_reactor::PreparedResult::Err {
                    stream: event.stream,
                    seq: before.0,
                    message: "managed feed failed".into(),
                };
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
            if self.peer_birth_error.is_some() {
                let result = ::aether_bloomery_reactor::PreparedResult::Err {
                    stream: event.stream,
                    seq: before.0,
                    message: "reactor peer failed to spawn".into(),
                };
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
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
            if !matches!(&self.feed.mode, ::aether_bloomery_reactor::FeedMode::Direct) {
                let result = ::aether_bloomery_reactor::PreparedResult::Err {
                    stream: batch.stream,
                    seq: before.0,
                    message: "external fold-only history is unavailable in managed mode".into(),
                };
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
            if self.peer_birth_error.is_some() {
                let result = ::aether_bloomery_reactor::PreparedResult::Err {
                    stream: batch.stream,
                    seq: before.0,
                    message: "reactor peer failed to spawn".into(),
                };
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
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

fn managed_handler_tokens(
    views: &Ident,
    names: &ManagedNames<'_>,
    peers: &[ReactorPeer],
) -> (TokenStream2, TokenStream2) {
    let begin = begin_handler_tokens(names, peers);
    let batch = live_batch_handler_tokens(names);
    let read = read_events_result_handler_tokens(names);
    let helpers = managed_helpers_tokens(views, names, peers);
    (quote! { #begin #batch #read }, helpers)
}

fn begin_handler_tokens(names: &ManagedNames<'_>, peers: &[ReactorPeer]) -> TokenStream2 {
    let ack_prepared_fn = names.ack_prepared;
    let request_fn = names.request;
    let ready_peers = peers.iter().map(|peer| {
        let ty = &peer.peer;
        let subname = &peer.subname;
        quote! { ctx.child_as::<#ty>(#subname).is_some() }
    });
    quote! {
        #[handler::manual]
        fn on_begin_warmup(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
            begin: ::aether_bloomery_reactor::BeginWarmup,
        ) {
            use ::aether_actor::OutboundReply;
            let peers_ready = true #(&& #ready_peers)*;
            let refusal = if !peers_ready || self.peer_birth_error.is_some() {
                Some("reactor peer failed to spawn")
            } else if self.cluster.owner().cursor().0 != 0
                || self.cluster.stream().is_some()
                || self.cluster.is_poisoned()
            {
                Some("warmup requires an empty direct cluster")
            } else {
                self.feed.begin(begin.stream.clone(), begin.journal_mailbox, begin.historical_through).err()
            };
            if let Some(message) = refusal {
                let result = ::aether_bloomery_reactor::PreparedResult::Err {
                    stream: begin.stream,
                    seq: self.cluster.owner().cursor().0,
                    message: message.into(),
                };
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
            self.cluster.bind_stream(begin.stream.clone());
            if begin.historical_through == 0 {
                let result = ::aether_bloomery_reactor::PreparedResult::ok(begin.stream, 0);
                #ack_prepared_fn(ctx, &self.ack, &result);
            } else {
                self.#request_fn(ctx);
            }
        }
    }
}

fn live_batch_handler_tokens(names: &ManagedNames<'_>) -> TokenStream2 {
    let ack_prepared_fn = names.ack_prepared;
    let drain_fn = names.drain;
    quote! {
        #[handler::manual]
        fn on_live_batch(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_, ::aether_actor::Manual>,
            batch: ::aether_bloomery_reactor::LiveEventBatch,
        ) {
            use ::aether_actor::OutboundReply;
            if !matches!(
                &self.feed.mode,
                ::aether_bloomery_reactor::FeedMode::Warming(_)
                    | ::aether_bloomery_reactor::FeedMode::Feeding { .. }
            ) {
                let result = ::aether_bloomery_reactor::PreparedResult::Err {
                    stream: batch.stream,
                    seq: self.cluster.owner().cursor().0,
                    message: "live batch requires an active managed feed".into(),
                };
                #ack_prepared_fn(ctx, &self.ack, &result);
                if ctx.reply_target().is_some() {
                    ctx.reply(&result);
                }
                return;
            }
            if batch.validate().is_err() {
                ctx.fatal_abort("invalid live batch range".into());
            }
            for entry in batch.entries {
                if let Err(reason) = self.feed.push_live(&batch.stream, entry) {
                    ctx.fatal_abort(reason.into());
                }
            }
            self.#drain_fn(ctx);
        }
    }
}

fn read_events_result_handler_tokens(names: &ManagedNames<'_>) -> TokenStream2 {
    let warmup_fn = names.warmup;
    let ack_prepared_fn = names.ack_prepared;
    let drain_fn = names.drain;
    let request_fn = names.request;
    let fail_fn = names.fail;
    quote! {
        #[handler::single]
        fn on_read_events_result(
            &mut self,
            ctx: &mut ::aether_actor::WasmCtx<'_>,
            reply: ::aether_bloomery_reactor::ReadEventsResult,
        ) {
            let Some(state) = (match &mut self.feed.mode {
                ::aether_bloomery_reactor::FeedMode::Warming(state) => Some(state),
                _ => None,
            }) else {
                self.#fail_fn(ctx, "unsolicited historical reply");
                return;
            };
            let Some(pending) = state.pending.take() else {
                self.#fail_fn(ctx, "unsolicited historical reply");
                return;
            };
            if ctx.in_reply_to().map(|id| id.0) != Some(pending.correlation)
                || ctx.source_mailbox() != Some(pending.source)
            {
                self.#fail_fn(ctx, "stale or wrong-source historical reply");
                return;
            }
            if self.cluster.owner().cursor().0 != pending.after {
                self.#fail_fn(ctx, "historical reply overlaps folded prefix");
                return;
            }
            let boundary = state.historical_through;
            let entries = match reply {
                ::aether_bloomery_reactor::ReadEventsResult::Err { after, .. } => {
                    self.#fail_fn(ctx, if after == pending.after {
                        "journal historical read failed"
                    } else {
                        "journal reply boundary mismatch"
                    });
                    return;
                }
                ::aether_bloomery_reactor::ReadEventsResult::Ok { after, head, entries } => {
                    if after != pending.after || head < boundary || entries.len() != pending.limit as usize {
                        self.#fail_fn(ctx, "malformed or short historical page");
                        return;
                    }
                    for (offset, entry) in entries.iter().enumerate() {
                        let expected = u64::try_from(offset).ok()
                            .and_then(|offset| pending.after.checked_add(offset))
                            .and_then(|seq| seq.checked_add(1));
                        if expected != Some(entry.seq) || entry.seq > boundary {
                            self.#fail_fn(ctx, "noncontiguous historical page");
                            return;
                        }
                    }
                    entries
                }
            };
            for entry in entries {
                if #warmup_fn(self.cluster.owner_mut(), entry).is_err() {
                    self.#fail_fn(ctx, "historical view fold failed");
                    return;
                }
            }
            self.cluster.trust_cursor();
            let result = ::aether_bloomery_reactor::PreparedResult::ok(
                self.cluster.stream().unwrap_or_default(),
                self.cluster.owner().cursor().0,
            );
            #ack_prepared_fn(ctx, &self.ack, &result);
            if self.cluster.owner().cursor().0 == boundary {
                self.feed.finish_history();
                self.#drain_fn(ctx);
            } else {
                self.#request_fn(ctx);
            }
        }
    }
}

fn managed_helpers_tokens(views: &Ident, names: &ManagedNames<'_>, peers: &[ReactorPeer]) -> TokenStream2 {
    let live_fn = names.live;
    let ack_prepared_fn = names.ack_prepared;
    let ack_evaluated_fn = names.ack_evaluated;
    let drain_fn = names.drain;
    let request_fn = names.request;
    let fail_fn = names.fail;
    let delivery = peer_delivery_tokens(peers, ack_evaluated_fn, &quote! { continue; });
    quote! {
        impl #views {
            fn #request_fn<M: ::aether_actor::ReplyMode>(&mut self, ctx: &mut ::aether_actor::WasmCtx<'_, M>) {
                use ::aether_actor::MailSender;
                let ::aether_bloomery_reactor::FeedMode::Warming(state) = &self.feed.mode else {
                    return;
                };
                let after = self.cluster.owner().cursor().0;
                let Some(remaining) = state.historical_through.checked_sub(after).filter(|remaining| *remaining > 0) else {
                    self.#fail_fn(ctx, "invalid historical read boundary");
                    return;
                };
                let limit = remaining.min(u64::from(::aether_bloomery_reactor::WARMUP_PAGE_LIMIT)) as u32;
                let journal_mailbox = state.journal_mailbox;
                ctx.send_to(journal_mailbox, &::aether_bloomery_reactor::ReadEvents { after, limit });
                let pending = ::aether_bloomery_reactor::PendingRead::new(
                    ctx.prev_correlation(),
                    journal_mailbox,
                    after,
                    limit,
                );
                let ::aether_bloomery_reactor::FeedMode::Warming(state) = &mut self.feed.mode else {
                    return;
                };
                state.pending = Some(pending);
            }

            fn #fail_fn<M: ::aether_actor::ReplyMode>(
                &mut self,
                ctx: &mut ::aether_actor::WasmCtx<'_, M>,
                reason: &str,
            ) {
                let stream = ::aether_actor::__macro_internals::String::from(self.feed.stream().unwrap_or_default());
                self.cluster.mark_poisoned();
                self.feed.mode = ::aether_bloomery_reactor::FeedMode::Failed;
                let result = ::aether_bloomery_reactor::PreparedResult::Err {
                    stream,
                    seq: self.cluster.owner().cursor().0,
                    message: reason.into(),
                };
                #ack_prepared_fn(ctx, &self.ack, &result);
            }

            fn #drain_fn<M: ::aether_actor::ReplyMode>(&mut self, ctx: &mut ::aether_actor::WasmCtx<'_, M>) {
                if !matches!(&self.feed.mode, ::aether_bloomery_reactor::FeedMode::Feeding { .. }) {
                    return;
                }
                while let Some(entry) = self.feed.queue.pop() {
                    let stream = ::aether_actor::__macro_internals::String::from(self.feed.stream().unwrap_or_default());
                    match #live_fn(self.cluster.owner_mut(), stream.clone(), entry) {
                        Ok(prepared) => {
                            self.cluster.trust_cursor();
                            let result = ::aether_bloomery_reactor::PreparedResult::ok(stream.clone(), prepared.seq);
                            #ack_prepared_fn(ctx, &self.ack, &result);
                            #delivery
                            if let Some(evaluated) = self.cluster.start_live(stream, prepared.seq, &expected) {
                                #ack_evaluated_fn(ctx, &self.ack, &evaluated);
                            }
                        }
                        Err(_) => {
                            self.#fail_fn(ctx, "live view fold failed");
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn peer_delivery_tokens(peers: &[ReactorPeer], ack_evaluated_fn: &Ident, on_missing: &TokenStream2) -> TokenStream2 {
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
            #on_missing
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
        pub struct #peer_ty {
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

#[cfg(test)]
mod rewritten_export_tests {
    use super::{COORDINATOR_IDENT, Envelope, NamespaceTok, ReactorPeer, rewritten_exports};
    use proc_macro2::TokenStream as TokenStream2;
    use quote::format_ident;
    use syn::{Type, parse_quote};

    fn tokens_contain(tokens: &TokenStream2, needle: &str) -> bool {
        tokens.to_string().replace(' ', "").contains(&needle.replace(' ', ""))
    }

    #[test]
    fn rewritten_exports_inserts_coordinator_and_peer_factories_once() {
        let probe: Type = parse_quote!(Probe);
        let publisher: Type = parse_quote!(Publisher);
        let witness: Type = parse_quote!(Witness);
        let sink: Type = parse_quote!(Sink);
        let reactors = [
            Envelope {
                ty: publisher.clone(),
                namespace: NamespaceTok::Lit(String::from("test.bloomery.export.publisher")),
                extensions: TokenStream2::new(),
                is_reactor: true,
            },
            Envelope {
                ty: witness.clone(),
                namespace: NamespaceTok::Lit(String::from("test.bloomery.export.witness")),
                extensions: TokenStream2::new(),
                is_reactor: true,
            },
        ];
        let reactor_refs: Vec<&Envelope> = reactors.iter().collect();
        let coordinator = format_ident!("{COORDINATOR_IDENT}");
        let peers = [
            ReactorPeer {
                reactor: publisher.clone(),
                peer: format_ident!("__AetherBloomeryReactorPeer_n1"),
                namespace: String::from("test.bloomery.export.publisher"),
                subname: String::from("r_1"),
            },
            ReactorPeer {
                reactor: witness.clone(),
                peer: format_ident!("__AetherBloomeryReactorPeer_n2"),
                namespace: String::from("test.bloomery.export.witness"),
                subname: String::from("r_2"),
            },
        ];

        let tokens = rewritten_exports(&[probe, publisher, witness, sink], &reactor_refs, &coordinator, &peers);
        assert!(tokens_contain(&tokens, "Probe"));
        assert!(tokens_contain(&tokens, COORDINATOR_IDENT));
        assert!(tokens_contain(&tokens, "__AetherBloomeryReactorPeer_n1"));
        assert!(tokens_contain(&tokens, "__AetherBloomeryReactorPeer_n2"));
        assert!(tokens_contain(&tokens, "Sink"));
        assert!(!tokens_contain(&tokens, "Publisher"));
        assert!(!tokens_contain(&tokens, "Witness"));
    }
}
