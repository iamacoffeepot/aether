//! `aether.inventory` cap (ADR-0088 §6). Serves the reverse-lookup
//! inventory over mail so an out-of-process observer (the MCP harness)
//! reads the running substrate's **own, per-build** inventory instead of
//! a drift-prone compiled-in copy.
//!
//! Two request kinds, both replying synchronously via `ctx.reply`:
//!
//! - [`Manifest`] → [`ManifestResult`]: the compile-time manifest —
//!   every link-time [`NameEntry`](aether_data::name_inventory::NameEntry)
//!   (declared mailbox namespaces + kinds + transforms) and every
//!   [`TemplateEntry`](aether_data::name_inventory::TemplateEntry)
//!   (instanced families). Templates ship their *family shape*
//!   (`Bounded` range / `Declared` domain / `Dynamic`) so the client
//!   expands or prehashes them locally — the manifest is NOT flattened to
//!   a hash → name map (ADR-0088 §6). The client folds this once at
//!   connect and reconstructs its own static reverse map.
//! - [`Resolve`] → [`ResolveResult`]: per-id reverse lookup of
//!   dynamically-minted instance ids the client can't compute from the
//!   manifest alone (the runtime-registry arm of the ADR-0088 §2 chain,
//!   `thread_name::resolve_runtime`). `None` on a miss so the client
//!   falls back to rendering the ADR-0064 tagged-id string itself.
//!
//! The cap holds no state — it reads the link-time inventories and the
//! process-global runtime registry directly at request time, both of
//! which are fixed (inventories) or write-rarely (registry) for the
//! process lifetime. `#[bridge(singleton)]` auto-submits its own
//! `NameEntry` for `NAMESPACE`, so `aether.inventory` reverses through
//! the same static map it serves.

use aether_kinds::{Manifest, ManifestResult, Resolve, ResolveResult};

#[aether_actor::bridge(singleton)]
mod native {
    use super::{Manifest, ManifestResult, Resolve, ResolveResult};

    use aether_actor::{MailCtx, actor};
    use aether_data::name_inventory::{ParamKind, name_entries, template_entries};
    use aether_data::tagged_id;
    use aether_kinds::{NameEntryWire, ParamKindWire, ResolvedName, TemplateEntryWire};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::runtime::thread_name::resolve_runtime;

    /// `aether.inventory` cap (ADR-0088 §6). Stateless — both handlers
    /// read process-global tables (the link-time inventories, the
    /// runtime registry) directly, so there is nothing to carry across
    /// handler calls.
    pub struct InventoryCapability;

    /// Project one link-time `ParamKind` onto its wire mirror. `Bounded`
    /// / `Declared` carry their range / domain so the client expands the
    /// family locally; `Dynamic` carries only the shape (its instances
    /// reverse via [`Resolve`]).
    fn param_kind_wire(param: &ParamKind) -> ParamKindWire {
        match *param {
            ParamKind::Bounded { lo, hi } => ParamKindWire::Bounded { lo, hi },
            ParamKind::Declared { domain } => ParamKindWire::Declared {
                domain: domain.to_vec(),
            },
            ParamKind::Dynamic => ParamKindWire::Dynamic,
        }
    }

    #[actor]
    impl NativeActor for InventoryCapability {
        type Config = ();

        /// ADR-0088 §6 chassis-owned mailbox. Registered on the desktop +
        /// headless chassis (via `with_common_caps`), matching `aether.fs`.
        const NAMESPACE: &'static str = "aether.inventory";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        /// Reply with the per-build reverse-lookup manifest: every
        /// declared name + every instanced-family template.
        ///
        /// # Agent
        /// Reply: `ManifestResult`. Carries `names` (declared mailbox
        /// namespaces, kinds, transforms) + `templates` (instanced
        /// families, preserving their `Bounded`/`Declared`/`Dynamic`
        /// shape). Fold `names` into a hash → name map and expand the
        /// `Bounded`/`Declared` templates locally; resolve `Dynamic`
        /// families per-id via `aether.inventory.resolve`.
        // Stateless cap — the manifest is read from the process-global
        // link-time inventories, not from `self`. `&mut self` is the
        // `#[handler]` dispatch signature, not a state read.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_manifest(&mut self, ctx: &mut NativeCtx<'_>, _mail: Manifest) {
            let names = name_entries()
                .map(|entry| NameEntryWire {
                    domain: entry.domain.to_vec(),
                    name: entry.name.into(),
                })
                .collect();
            let templates = template_entries()
                .map(|entry| TemplateEntryWire {
                    domain: entry.domain.to_vec(),
                    template: entry.template.into(),
                    param: param_kind_wire(&entry.param),
                })
                .collect();
            ctx.reply(&ManifestResult { names, templates });
        }

        /// Resolve each requested tagged-id string to its origin name via
        /// the runtime-registry arm of the reverse-lookup chain.
        ///
        /// # Agent
        /// Reply: `ResolveResult`. One `ResolvedName { id, name }` per
        /// requested id, in request order and echoing `id` for
        /// correlation. `name` is `Some` for a dynamically-minted
        /// instance the substrate has registered; `None` on a miss (or an
        /// unparseable id), at which point the caller renders the
        /// ADR-0064 tagged-id string itself. Call this only for ids a
        /// locally-folded manifest couldn't resolve.
        // Stateless cap — `resolve` reads the process-global runtime
        // registry, not `self`. `&mut self` is the `#[handler]` dispatch
        // signature, not a state read.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_resolve(&mut self, ctx: &mut NativeCtx<'_>, mail: Resolve) {
            let resolved = mail
                .ids
                .into_iter()
                .map(|id| {
                    // A malformed tagged-id string reports `None` rather
                    // than aborting the batch — one bad id doesn't sink
                    // its siblings.
                    let name = tagged_id::decode(&id).ok().and_then(resolve_runtime);
                    ResolvedName { id, name }
                })
                .collect();
            ctx.reply(&ResolveResult { resolved });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use aether_data::{
            MailboxId, SessionToken, ThreadId, Uuid, mailbox_id_from_name, thread_id_from_name,
        };
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::mail::registry::Registry;
        use aether_substrate::mail::{ReplyTarget, ReplyTo};
        use aether_substrate::runtime::thread_name::register;
        use serde::de::DeserializeOwned;
        use std::sync::Arc;
        use std::sync::mpsc::Receiver;
        use std::time::Duration;

        /// Cap + loopback mailer + transport wired so `ctx.reply`
        /// egresses as a `ToSession` event the test can decode. Mirrors
        /// the `trace.rs` `DispatchTracedFixture` shape.
        struct Fixture {
            rx: Receiver<EgressEvent>,
            transport: Arc<NativeBinding>,
            cap: InventoryCapability,
        }

        fn fixture() -> Fixture {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let (outbound, rx) = HubOutbound::attached_loopback();
            let mailer = Arc::new(
                Mailer::new(Arc::clone(&registry), Arc::clone(&store)).with_outbound(outbound),
            );
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0x1117),
            ));
            Fixture {
                rx,
                transport,
                cap: InventoryCapability,
            }
        }

        fn session_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_> {
            let sender = ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::nil())));
            NativeCtx::new(
                transport,
                sender,
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            )
        }

        fn decode_reply<K: aether_data::Kind + DeserializeOwned>(rx: &Receiver<EgressEvent>) -> K {
            let event = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("test: egress event arrives within 1s deadline");
            let EgressEvent::ToSession {
                kind_name, payload, ..
            } = event
            else {
                panic!("expected ToSession egress, got {event:?}");
            };
            assert_eq!(kind_name, K::NAME);
            postcard::from_bytes(&payload).expect("test: reply payload decodes via postcard")
        }

        /// The served manifest carries a known chassis mailbox name
        /// (`aether.fs`, a declared `NameEntry`) and a known instanced
        /// family (`aether-worker-{N}`, a `Bounded` `TemplateEntry`).
        /// Touching `FsCapability` forces its module — and the macro-
        /// auto-emitted `NameEntry` — into this unit-test binary; the
        /// substrate's `thread_name` module submits the worker template.
        #[test]
        fn manifest_contains_chassis_name_and_worker_template() {
            // Force `FsCapability`'s `NameEntry` submission to link.
            use crate::fs::FsCapability;
            use aether_actor::Actor;
            assert_eq!(FsCapability::NAMESPACE, "aether.fs");
            // Force the substrate's worker / root / instanced thread-name
            // templates to link by referencing the resolve chain.
            let _ = resolve_runtime(0);

            let mut fix = fixture();
            let mut ctx = session_ctx(&fix.transport);
            fix.cap.on_manifest(&mut ctx, Manifest {});
            drop(ctx);

            let result = decode_reply::<ManifestResult>(&fix.rx);
            assert!(
                result.names.iter().any(|n| n.name == "aether.fs"),
                "manifest should carry the aether.fs chassis mailbox NameEntry; names: {:?}",
                result.names.iter().map(|n| &n.name).collect::<Vec<_>>(),
            );
            assert!(
                result.templates.iter().any(|t| {
                    t.template == "aether-worker-{N}"
                        && matches!(t.param, ParamKindWire::Bounded { .. })
                }),
                "manifest should carry the aether-worker-{{N}} Bounded template; templates: {:?}",
                result
                    .templates
                    .iter()
                    .map(|t| &t.template)
                    .collect::<Vec<_>>(),
            );
        }

        /// A registered dynamic-instance id resolves to its name; an
        /// unregistered id and a malformed string both report `None`
        /// (the latter without sinking its siblings). Order + `id` echo
        /// are preserved.
        #[test]
        fn resolve_returns_registered_name_and_none_on_miss() {
            // Register a dynamic instance name the way the runtime name
            // builders do (a name no static template instantiates).
            let registered = ThreadId::from_name("aether-instanced-inventory-test:7");
            register(registered.0, "aether-instanced-inventory-test:7");
            let registered_tag =
                tagged_id::encode(registered.0).expect("ThreadId always tag-encodes");

            // An id the registry has never seen.
            let unseen = thread_id_from_name("aether-instanced-never-registered");
            let unseen_tag = tagged_id::encode(unseen.0).expect("ThreadId always tag-encodes");

            // A well-formed mailbox id that the runtime registry doesn't
            // hold (statics live in the static map, not the dynamic arm),
            // so `resolve_runtime` misses it -> None.
            let mailbox = mailbox_id_from_name("aether.fs");
            let mailbox_tag = tagged_id::encode(mailbox.0).expect("MailboxId tag-encodes");

            let mut fix = fixture();
            let mut ctx = session_ctx(&fix.transport);
            fix.cap.on_resolve(
                &mut ctx,
                Resolve {
                    ids: vec![
                        registered_tag.clone(),
                        unseen_tag.clone(),
                        mailbox_tag.clone(),
                        "not-a-tagged-id".to_string(),
                    ],
                },
            );
            drop(ctx);

            let result = decode_reply::<ResolveResult>(&fix.rx);
            assert_eq!(result.resolved.len(), 4, "one entry per requested id");

            assert_eq!(result.resolved[0].id, registered_tag);
            assert_eq!(
                result.resolved[0].name.as_deref(),
                Some("aether-instanced-inventory-test:7"),
                "registered dynamic instance reverses to its name",
            );

            assert_eq!(result.resolved[1].id, unseen_tag);
            assert_eq!(
                result.resolved[1].name, None,
                "unregistered id misses the runtime registry",
            );

            assert_eq!(result.resolved[2].id, mailbox_tag);
            assert_eq!(
                result.resolved[2].name, None,
                "a static name lives in the manifest, not the dynamic arm",
            );

            assert_eq!(result.resolved[3].id, "not-a-tagged-id");
            assert_eq!(
                result.resolved[3].name, None,
                "a malformed id reports None without aborting the batch",
            );
        }
    }
}
