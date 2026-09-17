//! `reactor_bundle!` — generate a views owner and one inline peer per reactor.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, LitStr, Token};

mod kw {
    syn::custom_keyword!(default);
    syn::custom_keyword!(namespace);
}

pub struct BundleDef {
    pub views: Ident,
    pub namespace: LitStr,
    pub reactors: Vec<Ident>,
}

struct ReactorPeer {
    reactor: Ident,
    peer: Ident,
    subname: String,
    namespace: String,
}

impl Parse for BundleDef {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        input.parse::<kw::default>()?;
        input.parse::<Token![=]>()?;
        let views = input.parse::<Ident>()?;
        input.parse::<Token![,]>()?;
        input.parse::<kw::namespace>()?;
        input.parse::<Token![=]>()?;
        let namespace = input.parse::<LitStr>()?;
        input.parse::<Token![,]>()?;
        let reactors = Punctuated::<Ident, Token![,]>::parse_terminated(input)?;
        let reactors: Vec<Ident> = reactors.into_iter().collect();
        if reactors.is_empty() {
            return Err(syn::Error::new(Span::call_site(), "reactor_bundle! needs at least one reactor type"));
        }
        if namespace.value().is_empty() {
            return Err(syn::Error::new_spanned(&namespace, "reactor_bundle! namespace must be non-empty"));
        }
        Ok(Self { views, namespace, reactors })
    }
}

pub fn expand(def: BundleDef) -> syn::Result<TokenStream2> {
    let BundleDef { views, namespace, reactors } = def;
    let ns = namespace.value();
    let peers = peer_definitions(&ns, reactors)?;

    let views_namespace = &ns;
    let live_fn = format_ident!("__aether_{views}_fold_live");
    let warmup_fn = format_ident!("__aether_{views}_fold_warmup");
    let emit_fn = format_ident!("__aether_{views}_emit_outputs");
    let ack_prepared_fn = format_ident!("__aether_{views}_ack_prepared");
    let ack_evaluated_fn = format_ident!("__aether_{views}_ack_evaluated");
    let peer_structs = peers.iter().map(|peer| expand_peer(&views, peer, &emit_fn));
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
    let live = live_fn_tokens(&live_fn, &peers);
    let warmup = warmup_fn_tokens(&warmup_fn, &peers);
    let emit = emit_outputs_fn(&emit_fn);
    let ack = ack_fn_tokens(&ack_prepared_fn, &ack_evaluated_fn);
    let event = event_handler_tokens(&live_fn, &ack_prepared_fn, &ack_evaluated_fn, &peers);
    let batch = batch_handler_tokens(&warmup_fn, &ack_prepared_fn);

    Ok(quote! {
        pub struct #views {
            cluster: ::aether_bloomery_reactor::Cluster,
            output: ::aether_actor::__macro_internals::String,
            ack: ::aether_actor::__macro_internals::String,
        }

        #[::aether_actor::actor]
        impl ::aether_actor::WasmActor for #views {
            type Config = ::aether_bloomery_reactor::ClusterConfig;
            const NAMESPACE: &'static str = #views_namespace;

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
    })
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

fn peer_definitions(namespace: &str, reactors: Vec<Ident>) -> syn::Result<Vec<ReactorPeer>> {
    let mut peers = Vec::with_capacity(reactors.len());
    let mut seen = Vec::new();
    for reactor in reactors {
        let subname = to_snake(&reactor.to_string());
        if seen.iter().any(|existing| existing == &subname) {
            return Err(syn::Error::new_spanned(&reactor, "reactor_bundle! reactor type names must be unique"));
        }
        seen.push(subname.clone());
        let peer = format_ident!("{reactor}Peer", span = reactor.span());
        let peer_namespace = format!("{namespace}.{subname}");
        peers.push(ReactorPeer { reactor, peer, subname, namespace: peer_namespace });
    }
    Ok(peers)
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
    let ReactorPeer { reactor, peer: peer_ty, namespace, .. } = peer;
    quote! {
        pub struct #peer_ty {
            reactor: #reactor,
            output: ::aether_actor::__macro_internals::String,
        }

        #[::aether_actor::actor(instanced, child_of(#views))]
        impl ::aether_actor::WasmActor for #peer_ty {
            type Config = ::aether_bloomery_reactor::ClusterConfig;
            const NAMESPACE: &'static str = #namespace;

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
                    Ok(mut owner) => match self.reactor.evaluate(&mut owner) {
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
            R::visit_arms(&mut emit);
        }
    }
}

fn to_snake(name: &str) -> String {
    let mut out = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
