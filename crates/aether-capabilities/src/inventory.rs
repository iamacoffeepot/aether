//! `aether.inventory` cap (ADR-0088 §6, widened by ADR-0091 §5). Serves
//! the per-build reverse-lookup inventory **and** the per-engine live
//! kind-schema registry view over mail so an out-of-process observer
//! (the MCP harness) reads the running substrate's **own, per-build**
//! state instead of a drift-prone compiled-in copy.
//!
//! Three request kinds, each replying synchronously via `ctx.reply`:
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
//! - [`ListKinds`] → [`ListKindsResult`] (ADR-0091): every
//!   [`KindId`](aether_data::KindId) currently registered in the
//!   substrate's `Registry`, with its full
//!   [`SchemaType`](aether_data::SchemaType). The harness folds the
//!   reply into a per-engine encode cache so a `send_mail` against a
//!   component-defined kind encodes correctly the moment the
//!   `aether.component.load` returns — no per-kind hand-promotion into
//!   `aether-kinds`.
//!
//! The cap holds a clone of the substrate's `Arc<Registry>` (taken in
//! `init` via `NativeInitCtx::mailer().registry()` — the same `Arc` the
//! component-host cap clones for `register_or_match_all`), so a
//! `load_component`'s registrations are visible to `ListKinds` the
//! moment they return; no event channel, no cache invalidation. The
//! manifest / resolve arms remain stateless reads of process-global
//! link-time tables. `#[bridge(singleton)]` auto-submits its own
//! `NameEntry` for `NAMESPACE`, so `aether.inventory` reverses through
//! the same static map it serves.

use aether_kinds::{ListKinds, ListKindsResult, Manifest, ManifestResult, Resolve, ResolveResult};

#[aether_actor::bridge(singleton)]
mod native {
    use super::{ListKinds, ListKindsResult, Manifest, ManifestResult, Resolve, ResolveResult};

    use aether_actor::{OutboundReply, actor};
    use aether_data::KindId;
    use aether_data::canonical::kind_id_from_parts;
    use aether_data::name_inventory::{Cardinality, ParamKind, name_entries, template_entries};
    use aether_data::tagged_id;
    use aether_kinds::{
        CardinalityWire, KindDescriptorWire, NameEntryWire, ParamKindWire, ResolvedName,
        TemplateEntryWire,
    };
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::runtime::thread_name::resolve_runtime;
    use std::sync::Arc;

    /// `aether.inventory` cap (ADR-0088 §6, widened by ADR-0091 §5). The
    /// `Manifest` and `Resolve` arms read process-global tables (the
    /// link-time inventories, the runtime registry) directly with no
    /// per-cap state. The `ListKinds` arm projects the substrate's
    /// shared `Arc<Registry>`, captured in `init` from the bench /
    /// chassis mailer — load-time registrations performed by
    /// `ComponentHostCapability` mutate the same `Arc<Registry>`, so the
    /// reply reflects whatever vocabulary the substrate currently holds
    /// without any cross-cap event channel.
    pub struct InventoryCapability {
        registry: Arc<Registry>,
    }

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

    /// Project one link-time `Cardinality` onto its wire mirror (ADR-0088
    /// §4 v2) — the orthogonal how-many axis the client surfaces verbatim.
    fn cardinality_wire(cardinality: &Cardinality) -> CardinalityWire {
        match *cardinality {
            Cardinality::Bounded(count) => CardinalityWire::Bounded { count },
            Cardinality::OnePer(entity) => CardinalityWire::OnePer {
                entity: entity.into(),
            },
            Cardinality::Unbounded => CardinalityWire::Unbounded,
        }
    }

    #[actor]
    impl NativeActor for InventoryCapability {
        type Config = ();

        /// ADR-0088 §6 chassis-owned mailbox. Registered on the desktop +
        /// headless chassis (via `with_common_caps`), matching `aether.fs`.
        const NAMESPACE: &'static str = "aether.inventory";

        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            // Clone the substrate's shared `Arc<Registry>` — the same
            // `Arc` `ComponentHostCapability` clones for
            // `register_or_match_all` at `component.rs:170`. The shared
            // `Arc` is the propagation channel per ADR-0091 §2: a
            // load-time registration is visible to `on_list_kinds` the
            // moment it returns.
            let registry = Arc::clone(ctx.mailer().registry());
            Ok(Self { registry })
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
        // The manifest is read from the process-global link-time
        // inventories — `self.registry` is only consulted by
        // `on_list_kinds`. `&mut self` is the `#[handler]` dispatch
        // signature, not a state read here.
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
                    // The wire form carries the full `prefix ++ template`
                    // pattern; the split is an internal const-construction
                    // detail (ADR-0099 §5/§6 forward-feed).
                    template: entry.pattern().into_owned(),
                    param: param_kind_wire(&entry.param),
                    cardinality: cardinality_wire(&entry.cardinality),
                })
                .collect();
            ctx.reply(&ManifestResult { names, templates });
        }

        /// Reply with the substrate's live kind vocabulary: every
        /// [`KindDescriptor`](aether_data::KindDescriptor) currently
        /// registered in the engine's `Registry`, projected onto the
        /// wire (id + name + postcard-encoded
        /// [`SchemaType`](aether_data::SchemaType)). ADR-0091 §1–§2.
        ///
        /// # Agent
        /// Reply: `ListKindsResult`. The harness folds this into a
        /// per-engine encode cache so a `send_mail` against a
        /// component-defined kind encodes correctly the moment the
        /// `aether.component.load` returns. Lazy-on-miss: the harness
        /// calls this on the first `send_mail` for an unknown kind
        /// name, then reuses the cached vocabulary until the next miss
        /// (no TTL, no background poll). The schema rides as opaque
        /// postcard bytes (`schema_postcard`) because `SchemaType` has
        /// no `Schema` impl of its own; decode it with
        /// `postcard::from_bytes::<SchemaType>(&desc.schema_postcard)`.
        #[handler]
        fn on_list_kinds(&mut self, ctx: &mut NativeCtx<'_>, _mail: ListKinds) {
            let kinds = self
                .registry
                .list_kind_descriptors()
                .into_iter()
                .map(|desc| {
                    // The schema rides as opaque postcard bytes — see
                    // `KindDescriptorWire` for the rationale. The
                    // serialization is infallible for `SchemaType`
                    // (no `Map<String, _>` non-string-key edge cases
                    // because every nested field is a derive output).
                    let schema_postcard = postcard::to_allocvec(&desc.schema)
                        .expect("SchemaType always postcard-encodes (ADR-0030 canonical form)");
                    KindDescriptorWire {
                        id: KindId(kind_id_from_parts(&desc.name, &desc.schema)),
                        name: desc.name,
                        schema_postcard,
                    }
                })
                .collect();
            ctx.reply(&ListKindsResult { kinds });
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
        use aether_substrate::mail::{Source, SourceAddr};
        use aether_substrate::runtime::thread_name::register;
        use std::sync::Arc;
        use std::sync::mpsc::Receiver;

        use crate::test_chassis::decode_reply;

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
                cap: InventoryCapability {
                    registry: Arc::clone(&registry),
                },
            }
        }

        fn session_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_> {
            let sender = Source::to(SourceAddr::Session(SessionToken(Uuid::nil())));
            NativeCtx::new(
                transport,
                sender,
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            )
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
            // The worker template is `Bounded` on both axes: a `Bounded`
            // `param` (enumerable integer hole) and a `Bounded` cardinality
            // (the prehashed instance ceiling) — ADR-0088 §4 v2.
            assert!(
                result.templates.iter().any(|t| {
                    t.template == "aether-worker-{N}"
                        && matches!(t.param, ParamKindWire::Bounded { .. })
                        && matches!(t.cardinality, CardinalityWire::Bounded { .. })
                }),
                "manifest should carry the aether-worker-{{N}} Bounded template; templates: {:?}",
                result
                    .templates
                    .iter()
                    .map(|t| &t.template)
                    .collect::<Vec<_>>(),
            );
        }

        /// ADR-0088 §4 v2: an instanced actor declaring `one_per` surfaces
        /// a `OnePer(<entity>)` cardinality in the manifest, so a consumer
        /// reads "trampoline = one mailbox per loaded component" instead of
        /// an opaque `Dynamic` family. `WasmTrampoline` is the canonical
        /// case (`#[bridge(instanced, one_per = "component")]`); touching
        /// its `NAMESPACE` forces the macro-emitted `TemplateEntry` into
        /// this test binary.
        #[test]
        fn manifest_surfaces_one_per_cardinality_for_trampoline() {
            use crate::trampoline::WasmTrampoline;
            use aether_actor::Actor;
            assert_eq!(WasmTrampoline::NAMESPACE, "aether.embedded");

            let mut fix = fixture();
            let mut ctx = session_ctx(&fix.transport);
            fix.cap.on_manifest(&mut ctx, Manifest {});
            drop(ctx);

            let result = decode_reply::<ManifestResult>(&fix.rx);
            let trampoline = result
                .templates
                .iter()
                .find(|t| t.template == "aether.embedded:{subname}")
                .expect("manifest should carry the trampoline instanced-family template");
            // Shape axis unchanged (opaque runtime string); cardinality
            // axis now names the entity each instance tracks.
            assert!(matches!(trampoline.param, ParamKindWire::Dynamic));
            assert!(
                matches!(&trampoline.cardinality, CardinalityWire::OnePer { entity } if entity == "component"),
                "trampoline cardinality should be OnePer(\"component\"); got {:?}",
                trampoline.cardinality,
            );
        }

        /// ADR-0091: `ListKinds` returns the substrate's authoritative
        /// kind vocabulary. A kind registered in the bench's `Registry`
        /// — emulating the `register_or_match_all` path
        /// `ComponentHostCapability::handle_load` follows — shows up in
        /// the reply with the matching `KindId`, name, and a
        /// postcard-encoded `SchemaType` that decodes back to the
        /// original schema. This is the live-projection ADR-0091
        /// requires: the same `Arc<Registry>` `component.rs` mutates is
        /// what the cap reads on every call.
        #[test]
        fn list_kinds_projects_a_registered_kind() {
            use aether_data::{KindDescriptor, KindId, SchemaType, canonical::kind_id_from_parts};

            let mut fix = fixture();

            // Register a fresh kind directly on the registry — same
            // entry point `register_or_match_all` walks per descriptor.
            // A `String`-shaped param keeps the schema lookup distinct
            // from any link-time entry the static vocabulary already
            // submits.
            let desc = KindDescriptor {
                name: "aether.test.list_kinds_projection".to_owned(),
                schema: SchemaType::String,
            };
            let expected_id = KindId(kind_id_from_parts(&desc.name, &desc.schema));
            // Use the bench's registry (the one the cap also cloned in
            // `fixture`) so the read sees what we just wrote.
            fix.cap
                .registry
                .register_kind_with_descriptor(desc.clone())
                .expect("register fresh kind");

            let mut ctx = session_ctx(&fix.transport);
            fix.cap.on_list_kinds(&mut ctx, ListKinds {});
            drop(ctx);

            let result = decode_reply::<ListKindsResult>(&fix.rx);
            let entry = result
                .kinds
                .iter()
                .find(|k| k.name == desc.name)
                .unwrap_or_else(|| {
                    panic!(
                        "ListKindsResult should carry the registered kind; names: {:?}",
                        result.kinds.iter().map(|k| &k.name).collect::<Vec<_>>(),
                    )
                });
            assert_eq!(entry.id, expected_id, "id matches kind_id_from_parts");
            let schema: SchemaType = postcard::from_bytes(&entry.schema_postcard)
                .expect("schema_postcard round-trips through postcard");
            assert!(
                matches!(schema, SchemaType::String),
                "schema decodes back to the originally registered SchemaType",
            );
        }

        /// A registered dynamic-instance id resolves to its name; an
        /// unregistered id and a malformed string both report `None`
        /// (the latter without sinking its siblings). Order + `id` echo
        /// are preserved.
        // Constructs a well-formed mailbox id the runtime registry never holds
        // to drive the miss path — incidental test data, not a real address.
        #[allow(clippy::disallowed_methods)]
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
