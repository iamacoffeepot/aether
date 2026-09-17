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
    let push_fn = format_ident!("__aether_{views}_push_and_prepare");
    let emit_fn = format_ident!("__aether_{views}_emit_outputs");
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
    let send_peers = peers.iter().map(|peer| {
        let ty = &peer.peer;
        let subname = &peer.subname;
        quote! {
            if let Some(peer) = ctx.child_as::<#ty>(#subname) {
                peer.send(ctx, &prepared);
            }
        }
    });
    let prepare = preparation_fn(&push_fn, &peers);
    let emit = emit_outputs_fn(&emit_fn);

    Ok(quote! {
        pub struct #views {
            owner: ::aether_bloomery_reactor::Owner,
            output: ::aether_actor::__macro_internals::String,
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
    })
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
                let Ok(mut owner) = prepared.into_owner::<#reactor>() else {
                    return;
                };
                let Ok(intents) = self.reactor.evaluate(&mut owner) else {
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
