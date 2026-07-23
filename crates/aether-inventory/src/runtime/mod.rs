//! The `aether.inventory` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `InventoryCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the reverse-lookup helper nests here as
//! a sibling file (`resolve.rs`) covered by the same gate.

// The moved `#[runtime] impl NativeActor for InventoryCapability` body
// names the `#[runtime]` attribute, the cap identity, and the input/reply
// kinds, which previously resolved at `mod.rs` root — now sourced here
// beside the body.
use aether_actor::runtime;

use super::{InventoryCapability, ListHandlers, ListKinds, Manifest, Resolve};

#[cfg(not(target_family = "wasm"))]
use super::{HandlersResult, ListKindsResult, ManifestResult, ResolveResult};

// The reverse-lookup helper, nested under this `runtime` directory so the
// one `mod runtime;` gate in the parent covers it (no per-sibling `#[cfg]`).
mod resolve;

pub use resolve::resolve_ids;

pub use aether_data::KindId;
pub use aether_data::canonical::kind_id_from_parts;
pub use aether_data::name_inventory::{ParamKind, handler_entries, name_entries, template_entries};
pub use aether_data::wire;
pub use aether_kinds::{HandlerEntryWire, KindDescriptorWire, NameEntryWire, ParamKindWire, TemplateEntryWire};
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
use std::collections::HashSet;

/// `aether.inventory` runtime state — a ZST, because the cap has none.
/// Every arm reads either a process-global link-time table or the
/// engine's `Registry` borrowed from the handler ctx.
///
/// It exists as a named type rather than `Self` because a struct-hosted
/// split identity (ADR-0122) requires one: `#[actor]` reads
/// `type State = Self` as a request for the *un-split* shape, whose
/// handlers take a `&mut self` receiver and whose runtime impls are not
/// gated on `feature = "runtime"` (`native_expand.rs:60`).
pub struct InventoryCapabilityState;

#[runtime]
impl NativeActor for InventoryCapability {
    /// The runtime state this identity boots into (ADR-0122 split) — a
    /// ZST; see [`InventoryCapabilityState`].
    type State = InventoryCapabilityState;

    type Config = ();

    /// ADR-0088 §6 chassis-owned mailbox. Registered on the desktop +
    /// headless chassis (via `with_full_stack_caps`), matching `aether.fs`.
    const NAMESPACE: &'static str = "aether.inventory";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<InventoryCapabilityState, BootError> {
        Ok(InventoryCapabilityState)
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
    // Read from the process-global link-time inventories.
    #[handler::single]
    fn on_manifest(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: Manifest) -> ManifestResult {
        let names = name_entries()
            .map(|entry| NameEntryWire { domain: entry.domain.to_vec(), name: entry.name.into() })
            .collect();
        let templates = template_entries()
            .map(|entry| TemplateEntryWire {
                domain: entry.domain.to_vec(),
                // The wire form carries the full `prefix ++ template`
                // pattern; the split is an internal const-construction
                // detail (ADR-0099 §5/§6 forward-feed).
                template: entry.pattern().into_owned(),
                // `Bounded` / `Declared` carry their range / domain so the
                // client expands the family locally; `Dynamic` carries only
                // the shape (its instances reverse via `Resolve`).
                param: match entry.param {
                    ParamKind::Bounded { lo, hi } => ParamKindWire::Bounded { lo, hi },
                    ParamKind::Declared { domain } => ParamKindWire::Declared { domain: domain.to_vec() },
                    ParamKind::Dynamic => ParamKindWire::Dynamic,
                },
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
    // The engine's live vocabulary is read straight off the ctx-borrowed
    // `Registry` — the same one `ComponentHostCapability` registers into,
    // so a `load_component`'s kinds are visible the moment it returns
    // (ADR-0091 §2) without the cap holding its own `Arc` clone.
    #[handler::single]
    fn on_list_kinds(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: ListKinds) -> ListKindsResult {
        let kinds = ctx
            .mailer()
            .registry()
            .list_kind_descriptors()
            .into_iter()
            .map(|desc| {
                // The schema rides as opaque wire bytes — see
                // `KindDescriptorWire` for the rationale. The
                // serialization is infallible for `SchemaType`
                // (no `Map<String, _>` non-string-key edge cases
                // because every nested field is a derive output).
                let schema_wire =
                    wire::to_vec(&desc.schema).expect("SchemaType always wire-encodes (ADR-0118 canonical form)");
                KindDescriptorWire {
                    id: KindId(kind_id_from_parts(&desc.name, &desc.schema)),
                    name: desc.name,
                    schema_wire,
                }
            })
            .collect();
        ListKindsResult { kinds }
    }

    /// Resolve each requested tagged-id string to its origin name,
    /// dispatching on the ADR-0064 tag to the table that owns the id's
    /// family — the process-global thread-name registry for `thr-…`, the
    /// engine's own `Registry` for `mbx-…` / `knd-…`.
    ///
    /// # Agent
    /// Reply: `ResolveResult`. One `ResolvedName { id, name }` per
    /// requested id, in request order and echoing `id` for
    /// correlation. `name` is `Some` for a runtime-minted thread,
    /// mailbox, or kind the engine has registered — including a
    /// component loaded at `aether.component/aether.embedded:NAME`,
    /// which no link-time manifest can carry; `None` on a miss (or an
    /// unparseable id), at which point the caller renders the
    /// ADR-0064 tagged-id string itself. Call this only for ids a
    /// locally-folded manifest couldn't resolve.
    #[handler::single]
    fn on_resolve(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Resolve) -> ResolveResult {
        ResolveResult { resolved: resolve_ids(ctx.mailer().registry(), mail.ids) }
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
    // Read from the process-global link-time inventory.
    //
    // Field-identical rows are deduped (ADR-0160 §Decision 2): two
    // `#[actor]` blocks can legitimately share one `NAMESPACE` — the
    // desktop `DesktopWindowCapability` and the headless companion both
    // claim `aether.window` and, linked into one desktop binary, each
    // submit the same `(namespace, id, name, reply)` handler rows into the
    // link-time-global inventory. The rows carry no per-instance state, so
    // equality-dedup is lossless and keeps `describe_handlers` from
    // double-reporting each window handler. `HashSet::insert` keeps the
    // first occurrence, preserving inventory order for the survivors.
    #[handler::single]
    fn on_handlers(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListHandlers) -> HandlersResult {
        let mut seen = HashSet::new();
        let handlers = handler_entries()
            .filter(|entry| seen.insert((entry.namespace, entry.id, entry.name, entry.reply)))
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
    use aether_actor::actor;
    use aether_data::name_inventory::HandlerEntry;
    use aether_data::tagged_id;
    use aether_data::{MailboxId, SessionToken, ThreadId, Uuid, mailbox_id_from_name, thread_id_from_name};
    use aether_kinds::ParamKindWire;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::{Registry, noop_handler};
    use aether_substrate::mail::{Source, SourceAddr};
    use aether_substrate::runtime::thread_name::{register, resolve_runtime};
    use std::sync::Arc;

    /// The stateless cap + a fully-wired test mailer + `NativeBinding`
    /// transport. Handlers are called directly and return their
    /// result; no egress channel decode needed (ADR-0112 `-> R`
    /// migration). The engine `Registry` reaches the handlers through
    /// the ctx's mailer, the same way it does on a live chassis.
    struct Fixture {
        transport: Arc<NativeBinding>,
        state: InventoryCapabilityState,
        /// The same `Registry` the handlers reach through the ctx — held
        /// so a test can seed it the way a live chassis registration would.
        registry: Arc<Registry>,
    }

    fn fixture() -> Fixture {
        let registry = Arc::new(Registry::new());
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x1117)));
        Fixture { transport, state: InventoryCapabilityState, registry }
    }

    fn session_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_> {
        let sender = Source::to(SourceAddr::Session(SessionToken(Uuid::nil())));
        NativeCtx::new(transport, sender, aether_data::MailId::NONE, aether_data::MailId::NONE)
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
        use aether_actor::Addressable;
        use aether_fs::FsCapability;
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
        // The worker template carries a `Bounded` `param` — an
        // enumerable integer hole the client expands locally (ADR-0088 §4).
        assert!(
            result
                .templates
                .iter()
                .any(|t| { t.template == "aether-worker-{N}" && matches!(t.param, ParamKindWire::Bounded { .. }) }),
            "manifest should carry the aether-worker-{{N}} Bounded template; templates: {:?}",
            result.templates.iter().map(|t| &t.template).collect::<Vec<_>>(),
        );
    }

    /// Each id family reverses through the table that owns it: a thread
    /// id through the process-global name registry, a mailbox id through
    /// the engine's own `Registry`. An id neither table holds, and a
    /// malformed string, both report `None` (the latter without sinking
    /// its siblings). Order + `id` echo are preserved.
    // Constructs a well-formed mailbox id no registry holds to drive the
    // miss path — incidental test data, not a real address.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn resolve_dispatches_each_id_family_to_its_table() {
        // Register a dynamic instance name the way the runtime name
        // builders do (a name no static template instantiates).
        let registered = ThreadId::from_name("aether-instanced-inventory-test:7");
        register(registered.0, "aether-instanced-inventory-test:7");
        let registered_tag = tagged_id::encode(registered.0).expect("ThreadId always tag-encodes");

        // An id the registry has never seen.
        let unseen = thread_id_from_name("aether-instanced-never-registered");
        let unseen_tag = tagged_id::encode(unseen.0).expect("ThreadId always tag-encodes");

        // A well-formed mailbox id no table holds -> None.
        let mailbox = mailbox_id_from_name("aether.never-registered");
        let mailbox_tag = tagged_id::encode(mailbox.0).expect("MailboxId tag-encodes");

        let mut fix = fixture();

        // A mailbox registered the way the spawn path registers a hosted
        // actor (`spawn.rs:470`, ADR-0099 §3): under a lineage-folded id,
        // carrying the rendered `/` address as its display name — which is
        // why the id is not `hash(name)` here either. No link-time manifest
        // carries this name, so the engine `Registry` is the only table
        // that can reverse it.
        let component = mailbox_id_from_name("aether.inventory-test.lineage-fold");
        fix.registry
            .try_register_inbox_with_id(component, "aether.component/aether.embedded:probe", noop_handler())
            .expect("fresh registry has no conflicting mailbox");
        let component_tag = tagged_id::encode(component.0).expect("MailboxId tag-encodes");

        let mut ctx = session_ctx(&fix.transport);
        let result = InventoryCapability::on_resolve(
            &mut fix.state,
            &mut ctx,
            Resolve {
                ids: vec![
                    registered_tag.clone(),
                    unseen_tag.clone(),
                    component_tag.clone(),
                    mailbox_tag.clone(),
                    "not-a-tagged-id".to_string(),
                ],
            },
        );
        drop(ctx);
        assert_eq!(result.resolved.len(), 5, "one entry per requested id");

        assert_eq!(result.resolved[0].id, registered_tag);
        assert_eq!(
            result.resolved[0].name.as_deref(),
            Some("aether-instanced-inventory-test:7"),
            "registered dynamic instance reverses to its name",
        );

        assert_eq!(result.resolved[1].id, unseen_tag);
        assert_eq!(result.resolved[1].name, None, "unregistered id misses the runtime registry");

        assert_eq!(result.resolved[2].id, component_tag);
        assert_eq!(
            result.resolved[2].name.as_deref(),
            Some("aether.component/aether.embedded:probe"),
            "a runtime-registered mailbox reverses through the engine registry",
        );

        assert_eq!(result.resolved[3].id, mailbox_tag);
        assert_eq!(result.resolved[3].name, None, "a mailbox id no registry holds reports None");

        assert_eq!(result.resolved[4].id, "not-a-tagged-id");
        assert_eq!(result.resolved[4].name, None, "a malformed id reports None without aborting the batch");
    }

    /// A native test cap with a synchronous `-> R` handler — the
    /// surface ADR-0109 §5 makes `aether.inventory.handlers` carry.
    /// Its `#[actor]` expansion submits a link-time `HandlerEntry`
    /// declaring `ProbeReq -> ProbeReply`.
    #[derive(serde::Serialize, serde::Deserialize, aether_data::Kind, aether_data::Schema, Debug, Clone)]
    #[kind(name = "aether.test.inventory_handlers.req")]
    struct ProbeReq {}

    #[derive(serde::Serialize, serde::Deserialize, aether_data::Kind, aether_data::Schema, Debug, Clone)]
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
        #[handler::single]
        fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _mail: ProbeReq) -> ProbeReply {
            ProbeReply {}
        }
    }

    // Two field-identical link-time `HandlerEntry` rows submitted directly
    // into the process-global inventory — the shape a desktop binary linking
    // both `aether.window` runtimes produces (ADR-0160 §Decision 2): the
    // desktop `DesktopWindowCapability` and the headless companion share
    // `NAMESPACE = "aether.window"` and each emit the same
    // `(namespace, id, name, reply)` rows. `HandlerEntry` holds only
    // `'static` data, so a bare `inventory::submit!` reproduces the duplicate
    // without standing up either cap.
    inventory::submit! {
        HandlerEntry {
            namespace: "aether.test.window_dedup",
            id: KindId(0x0D1D_0000_0000_0001),
            name: "aether.test.window_dedup.set_mode",
            reply: Some(KindId(0x0D1D_0000_0000_0002)),
        }
    }
    inventory::submit! {
        HandlerEntry {
            namespace: "aether.test.window_dedup",
            id: KindId(0x0D1D_0000_0000_0001),
            name: "aether.test.window_dedup.set_mode",
            reply: Some(KindId(0x0D1D_0000_0000_0002)),
        }
    }

    /// Two field-identical link-time `HandlerEntry` rows fold to a single
    /// served row. Guards the ADR-0160 §Decision 2 dedup: a desktop binary
    /// links both `aether.window` runtimes (the desktop cap + the headless
    /// companion), which submit identical `(namespace, id, name, reply)`
    /// rows into the link-time-global inventory; without the dedup
    /// `describe_handlers` double-reports every window handler.
    #[test]
    fn on_handlers_folds_field_identical_rows() {
        let mut fix = fixture();
        let mut ctx = session_ctx(&fix.transport);
        let result = InventoryCapability::on_handlers(&mut fix.state, &mut ctx, ListHandlers {});
        drop(ctx);

        let served = result.handlers.iter().filter(|h| h.namespace == "aether.test.window_dedup").count();
        assert_eq!(served, 1, "two field-identical HandlerEntry rows must fold to one served row; got {served}");
    }
}
