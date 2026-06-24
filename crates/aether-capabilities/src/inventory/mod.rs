//! `aether.inventory` cap (ADR-0088 §6, widened by ADR-0091 §5). Serves
//! the per-build reverse-lookup inventory **and** the per-engine live
//! kind-schema registry view over mail so an out-of-process observer
//! (the MCP harness) reads the running substrate's **own, per-build**
//! state instead of a drift-prone compiled-in copy.
//!
//! Four request kinds, each replying synchronously (ADR-0112 `-> R`):
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
//!   [`KindId`] currently registered in the
//!   substrate's `Registry`, with its full
//!   [`SchemaType`](aether_data::SchemaType). The harness folds the
//!   reply into a per-engine encode cache so a `send_mail` against a
//!   component-defined kind encodes correctly the moment the
//!   `aether.component.load` returns — no per-kind hand-promotion into
//!   `aether-kinds`.
//! - [`ListHandlers`] → [`HandlersResult`] (ADR-0109 §5): the native
//!   handler manifest — every `#[handler]`'s `{ namespace, input kind,
//!   reply kind }` across every native actor linked into the substrate,
//!   read from the link-time
//!   [`HandlerEntry`](aether_data::name_inventory::HandlerEntry)
//!   inventory the `#[actor]` macro populates. The native analogue of
//!   the wasm `aether.kinds.inputs` custom section: the harness folds
//!   the reply per `namespace` so a native cap (`aether.fs`,
//!   `aether.render`, …) surfaces its `In -> Out` the way
//!   `describe_component` surfaces a wasm component's.
//!
//! The cap holds a clone of the substrate's `Arc<Registry>` (taken in
//! `init` via `NativeInitCtx::mailer().registry()` — the same `Arc` the
//! component-host cap clones for `register_or_match_all`), so a
//! `load_component`'s registrations are visible to `ListKinds` the
//! moment they return; no event channel, no cache invalidation. The
//! manifest / resolve arms remain stateless reads of process-global
//! link-time tables. `#[actor(singleton)]` auto-submits its own
//! `NameEntry` for `NAMESPACE`, so `aether.inventory` reverses through
//! the same static map it serves.

// Handler-signature kinds must be importable at module root because
// `#[actor]` emits `impl HandlesKind<K> for InventoryCapability {}`
// markers always-on, outside the `feature = "runtime"` gate. The reply
// kinds are named only by the gated handler bodies, so they ride the
// runtime gate below.
use aether_kinds::{ListHandlers, ListKinds, Manifest, Resolve};

use aether_actor::actor;

#[cfg(not(target_arch = "wasm32"))]
mod manifest;
#[cfg(not(target_arch = "wasm32"))]
mod resolve;

/// `aether.inventory` cap **identity** (ADR-0122 identity/runtime split,
/// ADR-0088 §6, widened by ADR-0091 §5). A ZST carrying only the
/// addressing — the `Addressable` / `HandlesKind` markers and the
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`InventoryCapabilityState`, holding the
/// substrate `Arc<Registry>` the `ListKinds` arm projects) lives behind
/// the one `feature = "runtime"` gate, so a transport-only build never
/// names it nor pulls `aether_substrate` through this cap.
///
/// The `Manifest` and `Resolve` arms read process-global tables (the
/// link-time inventories, the runtime registry) directly with no
/// per-cap state. The `ListKinds` arm projects the substrate's shared
/// `Arc<Registry>`, captured in `init` from the bench / chassis mailer —
/// load-time registrations performed by `ComponentHostCapability` mutate
/// the same `Arc<Registry>`, so the reply reflects whatever vocabulary
/// the substrate currently holds without any cross-cap event channel.
pub struct InventoryCapability;

// The reply kinds ride the native gate (not `runtime`): the `#[actor]`
// macro's ADR-0109 `HandlerEntry` inventory submission — emitted on every
// native build, runtime or not — names each handler's reply kind `::ID`,
// so a transport-only build must still see them. The wire-projection
// helpers, the `aether_substrate`-typed imports, and the state struct
// sit behind the one `feature = "runtime"` gate.
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::{HandlersResult, ListKindsResult, ManifestResult, ResolveResult};

#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

/// The `aether.inventory` runtime half (ADR-0122 identity/runtime split):
/// the wire-projection helpers, the `aether_substrate`-typed imports, and
/// the state struct, gated once by this module rather than per-import. The
/// `#[actor] impl` reaches them through the single `use runtime::*` glob
/// above.
#[cfg(feature = "runtime")]
mod runtime {
    pub use super::manifest::{cardinality_wire, param_kind_wire};
    pub use super::resolve::resolve_ids;

    pub use aether_data::KindId;
    pub use aether_data::canonical::kind_id_from_parts;
    pub use aether_data::name_inventory::{handler_entries, name_entries, template_entries};
    pub use aether_data::wire;
    pub use aether_kinds::{
        HandlerEntryWire, KindDescriptorWire, NameEntryWire, TemplateEntryWire,
    };
    pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    pub use aether_substrate::chassis::error::BootError;
    pub use aether_substrate::mail::registry::Registry;
    pub use std::sync::Arc;

    /// `aether.inventory` runtime state (ADR-0091 §2). Holds the substrate's
    /// shared `Arc<Registry>` — the same `Arc` `ComponentHostCapability`
    /// clones for `register_or_match_all` — so a load-time registration is
    /// visible to `on_list_kinds` the moment it returns. The `Manifest` /
    /// `Resolve` / `ListHandlers` arms are stateless reads of process-global
    /// link-time tables. The addressing identity is the distinct ZST
    /// `InventoryCapability`.
    pub struct InventoryCapabilityState {
        pub(super) registry: Arc<Registry>,
    }
}

// Used only by the test suite — gate to avoid unused-import warnings in
// the non-test build (they flow in via `use super::*` inside `mod tests`).
#[cfg(all(test, feature = "runtime"))]
use aether_data::tagged_id;
#[cfg(all(test, feature = "runtime"))]
use aether_kinds::{CardinalityWire, ParamKindWire};
#[cfg(all(test, feature = "runtime"))]
use aether_substrate::runtime::thread_name::resolve_runtime;

#[actor(singleton)]
impl NativeActor for InventoryCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// shared substrate `Arc<Registry>` the `ListKinds` arm projects.
    type State = InventoryCapabilityState;

    type Config = ();

    /// ADR-0088 §6 chassis-owned mailbox. Registered on the desktop +
    /// headless chassis (via `with_common_caps`), matching `aether.fs`.
    const NAMESPACE: &'static str = "aether.inventory";

    fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<InventoryCapabilityState, BootError> {
        // Clone the substrate's shared `Arc<Registry>` — the same
        // `Arc` `ComponentHostCapability` clones for
        // `register_or_match_all` at `component.rs:170`. The shared
        // `Arc` is the propagation channel per ADR-0091 §2: a
        // load-time registration is visible to `on_list_kinds` the
        // moment it returns.
        let registry = Arc::clone(ctx.mailer().registry());
        Ok(InventoryCapabilityState { registry })
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
    // inventories — `state.registry` is only consulted by
    // `on_list_kinds`, so this arm takes `_state`.
    #[handler]
    fn on_manifest(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: Manifest,
    ) -> ManifestResult {
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
        ManifestResult { names, templates }
    }

    /// Reply with the substrate's live kind vocabulary: every
    /// [`KindDescriptor`](aether_data::KindDescriptor) currently
    /// registered in the engine's `Registry`, projected onto the
    /// wire (id + name + wire-encoded
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
    /// wire bytes (`schema_wire`) because `SchemaType` has
    /// no `Schema` impl of its own; decode it with
    /// `wire::from_bytes::<SchemaType>(&desc.schema_wire)`.
    #[handler]
    fn on_list_kinds(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: ListKinds,
    ) -> ListKindsResult {
        let kinds = state
            .registry
            .list_kind_descriptors()
            .into_iter()
            .map(|desc| {
                // The schema rides as opaque wire bytes — see
                // `KindDescriptorWire` for the rationale. The
                // serialization is infallible for `SchemaType`
                // (no `Map<String, _>` non-string-key edge cases
                // because every nested field is a derive output).
                let schema_wire = wire::to_vec(&desc.schema)
                    .expect("SchemaType always wire-encodes (ADR-0118 canonical form)");
                KindDescriptorWire {
                    id: KindId(kind_id_from_parts(&desc.name, &desc.schema)),
                    name: desc.name,
                    schema_wire,
                }
            })
            .collect();
        ListKindsResult { kinds }
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
    // Stateless arm — `resolve` reads the process-global runtime
    // registry, not the cap state, so it takes `_state`.
    #[handler]
    fn on_resolve(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: Resolve,
    ) -> ResolveResult {
        ResolveResult {
            resolved: resolve_ids(mail.ids),
        }
    }

    /// Reply with the native handler manifest (ADR-0109 §5): every
    /// `#[handler]` across every native actor linked into the
    /// substrate, each carrying its owning `namespace`, input kind
    /// (id + name), and declared reply kind id. Read from the
    /// process-global link-time
    /// [`HandlerEntry`](aether_data::name_inventory::HandlerEntry)
    /// inventory the `#[actor]` macro populates — the native
    /// analogue of the wasm `aether.kinds.inputs` custom section.
    ///
    /// # Agent
    /// Reply: `HandlersResult`. One `HandlerEntryWire` per native
    /// handler; `reply` is the kind a `-> R` handler answers with
    /// (`None` for a fire-and-forget `-> ()` handler). Fold per
    /// `namespace` to read each native cap (`aether.fs`,
    /// `aether.render`, …) as a `describe_component`-style
    /// `In -> Out` handler list.
    // The manifest is read from the process-global link-time
    // inventory, so this arm takes `_state`.
    #[handler]
    fn on_handlers(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: ListHandlers,
    ) -> HandlersResult {
        let handlers = handler_entries()
            .map(|entry| HandlerEntryWire {
                namespace: entry.namespace.into(),
                id: entry.id,
                name: entry.name.into(),
                reply: entry.reply,
            })
            .collect();
        HandlersResult { handlers }
    }
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::*;
    use aether_data::{
        MailboxId, SessionToken, ThreadId, Uuid, mailbox_id_from_name, thread_id_from_name,
    };
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::mail::{Source, SourceAddr};
    use aether_substrate::runtime::thread_name::register;
    use std::sync::Arc;

    /// Runtime state + fully-wired test mailer + `NativeBinding`
    /// transport. Handlers are called directly and return their
    /// result; no egress channel decode needed (ADR-0112 `-> R`
    /// migration).
    struct Fixture {
        transport: Arc<NativeBinding>,
        state: InventoryCapabilityState,
    }

    fn fixture() -> Fixture {
        let registry = Arc::new(Registry::new());
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0x1117),
        ));
        Fixture {
            transport,
            state: InventoryCapabilityState {
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
        use aether_actor::Addressable;
        assert_eq!(FsCapability::NAMESPACE, "aether.fs");
        // Force the substrate's worker / root / instanced thread-name
        // templates to link by referencing the resolve chain.
        let _ = resolve_runtime(0);

        let mut fix = fixture();
        let mut ctx = session_ctx(&fix.transport);
        let result = InventoryCapability::on_manifest(&mut fix.state, &mut ctx, Manifest {});
        drop(ctx);

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
        use aether_actor::Addressable;
        assert_eq!(WasmTrampoline::NAMESPACE, "aether.embedded");

        let mut fix = fixture();
        let mut ctx = session_ctx(&fix.transport);
        let result = InventoryCapability::on_manifest(&mut fix.state, &mut ctx, Manifest {});
        drop(ctx);

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
    /// wire-encoded `SchemaType` that decodes back to the
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
        fix.state
            .registry
            .register_kind_with_descriptor(desc.clone())
            .expect("register fresh kind");

        let mut ctx = session_ctx(&fix.transport);
        let result = InventoryCapability::on_list_kinds(&mut fix.state, &mut ctx, ListKinds {});
        drop(ctx);

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
        let schema: SchemaType =
            wire::from_bytes(&entry.schema_wire).expect("schema_wire round-trips through wire");
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
        let registered_tag = tagged_id::encode(registered.0).expect("ThreadId always tag-encodes");

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
        let result = InventoryCapability::on_resolve(
            &mut fix.state,
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

    /// A native test cap with a synchronous `-> R` handler — the
    /// surface ADR-0109 §5 makes `aether.inventory.handlers` carry.
    /// Its `#[actor]` expansion submits a link-time `HandlerEntry`
    /// declaring `ProbeReq -> ProbeReply`.
    #[derive(
        serde::Serialize, serde::Deserialize, aether_data::Kind, aether_data::Schema, Debug, Clone,
    )]
    #[kind(name = "aether.test.inventory_handlers.req")]
    struct ProbeReq {}

    #[derive(
        serde::Serialize, serde::Deserialize, aether_data::Kind, aether_data::Schema, Debug, Clone,
    )]
    #[kind(name = "aether.test.inventory_handlers.reply")]
    struct ProbeReply {}

    struct ReplyProbeCap;

    #[actor]
    impl NativeActor for ReplyProbeCap {
        type Config = ();
        const NAMESPACE: &'static str = "aether.test.inventory_handlers.probe";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        /// A synchronous `-> ProbeReply` handler — the reply contract
        /// the link-time inventory captures. Stateless: the link-time
        /// `HandlerEntry` is what the test reads, not handler state.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _mail: ProbeReq) -> ProbeReply {
            ProbeReply {}
        }
    }

    /// ADR-0109 §5: a native cap's `-> R` handler surfaces `In -> Out`
    /// through `aether.inventory.handlers`. `on_handlers` projects the
    /// process-global link-time `HandlerEntry` inventory onto
    /// `HandlersResult`; the `ReplyProbeCap` entry round-trips its
    /// input kind (id + name) and its `Some(ProbeReply)` reply
    /// contract.
    #[test]
    fn handlers_surfaces_native_reply_contract() {
        use aether_actor::Addressable;
        use aether_data::Kind;
        // Force `ReplyProbeCap`'s `#[actor]` HandlerEntry submission to
        // link into this test binary.
        assert_eq!(
            ReplyProbeCap::NAMESPACE,
            "aether.test.inventory_handlers.probe"
        );

        let mut fix = fixture();
        let mut ctx = session_ctx(&fix.transport);
        let result = InventoryCapability::on_handlers(&mut fix.state, &mut ctx, ListHandlers {});
        drop(ctx);

        let entry = result
            .handlers
            .iter()
            .find(|h| h.namespace == ReplyProbeCap::NAMESPACE && h.id == <ProbeReq as Kind>::ID)
            .unwrap_or_else(|| {
                panic!(
                    "HandlersResult should carry the ReplyProbeCap probe handler; \
                         namespaces: {:?}",
                    result
                        .handlers
                        .iter()
                        .map(|h| &h.namespace)
                        .collect::<Vec<_>>(),
                )
            });
        assert_eq!(
            entry.name,
            <ProbeReq as Kind>::NAME,
            "input kind name round-trips"
        );
        assert_eq!(
            entry.reply,
            Some(<ProbeReply as Kind>::ID),
            "a `-> ProbeReply` handler surfaces ProbeReply as its reply (In -> Out)",
        );
    }
}
