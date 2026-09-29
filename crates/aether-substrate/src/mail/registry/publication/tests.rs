//! Tests for the publication table and its admission step (ADR-0241 §3/§4).
//!
//! The rule tests run [`admit`] over hand-built surfaces, so each names one
//! rule without wasm. The native test reads the real link-time inventory, and
//! the registry tests publish checked-in WAT modules through the owner, so
//! the table they exercise is the one a load stages, and the short-path index
//! they resolve through is the one that publish republishes.

use std::any::{TypeId, type_name};
use std::fmt::Write as _;
use std::iter;
use std::sync::Arc;
use std::time::Duration;

use aether_actor::{Addressable, One};
use aether_data::name_inventory::{
    ChildEntry, NameEntry, NativeTypeEntry, ParamKind, RootEntry, TemplateEntry, inventory,
};
use aether_data::{
    ACTOR_LINEAGE_SECTION, ACTOR_LINEAGE_SECTION_VERSION, ActorId, ActorLineageRecord, Blob, BlobHash,
    CONTENT_ADDRESSED_SECTION, ErasedActorPath, INPUTS_SECTION, INPUTS_SECTION_VERSION, InputsRecord, MAILBOX_DOMAIN,
    MailboxCategory, PRIVATE_INPUTS_SECTION, ReplyContract, THREAD_DOMAIN, wire,
};
use aether_kinds::{ComponentCapabilities, FallbackCapability, HandlerCapability};
use wasmtime::Engine;

use super::{AdmissionRefusal, Admitted, ModuleSurface, NativeHoldRefusal, NativeType, PublicationTable, admit};
use crate::actor::native::BlobCheckIn;
use crate::actor::wasm::module::{Module, ModuleCache};
use crate::config::RegistryQueueCapacities;
use crate::mail::KindId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{EffectBatch, RegistryApplied, RegistryBatch, RegistryEffect, RegistryEffectError};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{
    AddressResolutionError, AdoptRefused, ContractBreak, Registry, RouteContract, canonical_mailbox_id,
    lineage_mailbox_id, noop_handler,
};
use crate::scheduler::WakeSink;
use crate::store::BlobStore;
use crate::testing::boot_authority as auth;

const NATIVE_SINGLETON: &str = "test.publication.native_singleton";
const NATIVE_INSTANCED: &str = "test.publication.native_instanced";
const THREAD_NAMED: &str = "test.publication.thread_named";
const OTHER_TEMPLATE: &str = "test.publication.other_template";
/// A native root whose one declared instanced child is [`NATIVE_INSTANCED`],
/// so `NATIVE_ROOT/:k` is a native short path.
const NATIVE_ROOT: &str = "test.publication.native_root";

inventory::submit! { NameEntry { domain: MAILBOX_DOMAIN, name: NATIVE_SINGLETON } }
inventory::submit! {
    TemplateEntry { domain: MAILBOX_DOMAIN, prefix: NATIVE_INSTANCED, template: ":{subname}", param: ParamKind::Dynamic }
}
inventory::submit! { NameEntry { domain: THREAD_DOMAIN, name: THREAD_NAMED } }
inventory::submit! {
    TemplateEntry { domain: MAILBOX_DOMAIN, prefix: OTHER_TEMPLATE, template: "-{subname}", param: ParamKind::Dynamic }
}
inventory::submit! { NameEntry { domain: MAILBOX_DOMAIN, name: NATIVE_ROOT } }
inventory::submit! { RootEntry { actor: ActorId::singleton(NATIVE_ROOT), namespace: NATIVE_ROOT } }
inventory::submit! {
    ChildEntry {
        parent: ActorId::singleton(NATIVE_ROOT),
        child: ActorId::singleton(NATIVE_INSTANCED),
        parent_namespace: NATIVE_ROOT,
        child_namespace: NATIVE_INSTANCED,
    }
}

/// Two linked types sharing [`NATIVE_SINGLETON`], the way a chassis picks one
/// of several interchangeable capabilities, and a type linked nowhere.
struct SharedFirst;
struct SharedSecond;
struct Unlinked;

inventory::submit! {
    NativeTypeEntry { namespace: NATIVE_SINGLETON, type_id: TypeId::of::<SharedFirst>, type_name: type_name::<SharedFirst> }
}
inventory::submit! {
    NativeTypeEntry {
        namespace: NATIVE_SINGLETON,
        type_id: TypeId::of::<SharedSecond>,
        type_name: type_name::<SharedSecond>,
    }
}

const KEPT: KindId = KindId(0x11);
const ADDED: KindId = KindId(0x12);

fn hash(byte: u8) -> BlobHash {
    BlobHash::from_bytes([byte; 32])
}

/// A contract of `rows`, each replying nothing, with a fallback when asked.
fn contract(rows: &[KindId], fallback: bool) -> RouteContract {
    RouteContract::from_capabilities(&capabilities(rows, fallback))
}

fn capabilities(rows: &[KindId], fallback: bool) -> ComponentCapabilities {
    ComponentCapabilities {
        handlers: rows
            .iter()
            .map(|id| HandlerCapability { id: *id, name: id.to_string(), doc: None, reply: ReplyContract::None })
            .collect(),
        fallback: fallback.then_some(FallbackCapability { doc: None }),
        ..ComponentCapabilities::default()
    }
}

fn surface(exported: &[(&str, RouteContract)], private: &[(&str, RouteContract)]) -> ModuleSurface {
    let owned = |types: &[(&str, RouteContract)]| {
        types.iter().map(|(namespace, contract)| (Arc::from(*namespace), contract.clone())).collect()
    };
    ModuleSurface::new(owned(exported), owned(private))
}

/// Admit `candidate` against `published`, the modules currently holding
/// namespaces, each holding every namespace it exports.
fn admit_over(
    published: &[(BlobHash, &ModuleSurface)],
    hash: BlobHash,
    candidate: &ModuleSurface,
) -> Result<Admitted, AdmissionRefusal> {
    admit(
        hash,
        candidate,
        |namespace| {
            published
                .iter()
                .find(|(_, surface)| surface.exported_namespaces().any(|exported| &**exported == namespace))
                .map(|(hash, surface)| super::Holder::Module { hash: *hash, surface })
        },
        |_| None,
    )
}

// Catches: a native set built from the wrong domain or template filter, so a
// module takes a namespace the binary publishes, or is refused one it does
// not.
#[test]
fn a_native_namespace_is_refused_to_every_module() {
    let table = PublicationTable::native();
    let admit_one = |namespace: &str| {
        let candidate = surface(&[(namespace, contract(&[KEPT], false))], &[]);
        admit(hash(1), &candidate, |namespace| table.holder(namespace), |_| None)
    };

    for native in [NATIVE_SINGLETON, NATIVE_INSTANCED] {
        assert_eq!(admit_one(native), Err(AdmissionRefusal::NativeNamespace { namespace: Arc::from(native) }));
    }
    for unpublished in [THREAD_NAMED, OTHER_TEMPLATE] {
        assert_eq!(admit_one(unpublished), Ok(Admitted::Publish), "{unpublished} is not a native actor namespace");
    }
}

// Catches: native rows grouped by type rather than by namespace, which splits
// one shared namespace into two publications, or a hold that admits a second
// type sharing it or a type linked nowhere.
#[test]
fn a_shared_native_namespace_is_held_by_its_first_birth() {
    let mut table = PublicationTable::native();
    let (types, held) = table.native_publication(NATIVE_SINGLETON).expect("the namespace is native");
    assert!(types.contains(&TypeId::of::<SharedFirst>()) && types.contains(&TypeId::of::<SharedSecond>()));
    assert_eq!(held, None);

    table.hold(NATIVE_SINGLETON, NativeType::of::<SharedSecond>()).expect("the first birth holds");
    table.hold(NATIVE_SINGLETON, NativeType::of::<SharedSecond>()).expect("the holder is born again");
    assert!(matches!(
        table.hold(NATIVE_SINGLETON, NativeType::of::<SharedFirst>()),
        Err(NativeHoldRefusal::HeldByOther { .. })
    ));

    let mut fresh = PublicationTable::native();
    assert!(matches!(
        fresh.hold(NATIVE_SINGLETON, NativeType::of::<Unlinked>()),
        Err(NativeHoldRefusal::NotLinked { .. })
    ));
}

// Catches: a predecessor taken as the module being replaced rather than every
// holder of the candidate's namespaces, so exports shrink.
#[test]
fn a_republish_that_drops_a_namespace_is_refused() {
    let predecessor = surface(&[("test.a", contract(&[KEPT], false)), ("test.b", contract(&[KEPT], false))], &[]);
    let dropping = surface(&[("test.a", contract(&[KEPT], false))], &[]);
    let adding = surface(
        &[
            ("test.a", contract(&[KEPT], false)),
            ("test.b", contract(&[KEPT], false)),
            ("test.c", contract(&[KEPT], false)),
        ],
        &[],
    );

    assert_eq!(
        admit_over(&[(hash(1), &predecessor)], hash(2), &dropping),
        Err(AdmissionRefusal::DroppedNamespace { namespace: Arc::from("test.b") })
    );
    assert_eq!(admit_over(&[(hash(1), &predecessor)], hash(2), &adding), Ok(Admitted::Publish));
}

// Catches: the growth rule skipped, or reading the candidate as the
// predecessor, so a republish drops or changes a row or drops the fallback.
#[test]
fn a_republish_that_narrows_a_contract_is_refused() {
    let predecessor = surface(&[("test.a", contract(&[KEPT], true))], &[]);
    let narrowed = |successor: RouteContract| surface(&[("test.a", successor)], &[]);
    let changed_reply = RouteContract::from_capabilities(&ComponentCapabilities {
        handlers: vec![HandlerCapability { id: KEPT, name: String::new(), doc: None, reply: ReplyContract::Manual }],
        fallback: Some(FallbackCapability { doc: None }),
        ..ComponentCapabilities::default()
    });
    let refusal = |contract_break| AdmissionRefusal::ContractNarrowed {
        namespace: Arc::from("test.a"),
        contract_break,
        kind_name: None,
    };

    for (successor, contract_break) in [
        (contract(&[], true), ContractBreak::Row(KEPT)),
        (changed_reply, ContractBreak::Row(KEPT)),
        (contract(&[KEPT], false), ContractBreak::Fallback),
    ] {
        assert_eq!(admit_over(&[(hash(1), &predecessor)], hash(2), &narrowed(successor)), Err(refusal(contract_break)));
    }
    assert_eq!(
        admit_over(&[(hash(1), &predecessor)], hash(2), &narrowed(contract(&[KEPT, ADDED], true))),
        Ok(Admitted::Publish)
    );
}

// Catches: the #6845 gap reopening, where a republish drops or narrows an
// inline child type and its alias keeps advertising rows nothing handles.
#[test]
fn a_republish_holds_the_predecessors_private_types_to_growth() {
    let exported = [("test.parent", contract(&[KEPT], false))];
    let predecessor = surface(&exported, &[("test.child", contract(&[KEPT], false))]);
    let published = [(hash(1), &predecessor)];

    assert_eq!(
        admit_over(&published, hash(2), &surface(&exported, &[])),
        Err(AdmissionRefusal::DroppedPrivateType { namespace: Arc::from("test.child") })
    );
    assert_eq!(
        admit_over(&published, hash(2), &surface(&exported, &[("test.child", contract(&[], false))])),
        Err(AdmissionRefusal::PrivateContractNarrowed {
            namespace: Arc::from("test.child"),
            contract_break: ContractBreak::Row(KEPT),
            kind_name: None,
        })
    );

    let promoted = surface(&[exported[0].clone(), ("test.child", contract(&[KEPT], false))], &[]);
    assert_eq!(admit_over(&published, hash(2), &promoted), Ok(Admitted::Publish));
}

// Catches: predecessors collected from the first held namespace only, so a
// candidate that absorbs two modules may drop the second one's namespaces.
#[test]
fn a_candidate_covering_two_predecessors_must_cover_both() {
    let first = surface(&[("test.a", contract(&[KEPT], false)), ("test.b", contract(&[KEPT], false))], &[]);
    let second = surface(&[("test.c", contract(&[KEPT], false))], &[]);
    let published = [(hash(1), &first), (hash(2), &second)];
    let covering = |namespaces: &[&str]| {
        let exported: Vec<_> = namespaces.iter().map(|namespace| (*namespace, contract(&[KEPT], false))).collect();
        surface(&exported, &[])
    };

    assert_eq!(admit_over(&published, hash(3), &covering(&["test.a", "test.b", "test.c"])), Ok(Admitted::Publish));
    assert_eq!(
        admit_over(&published, hash(3), &covering(&["test.a", "test.c"])),
        Err(AdmissionRefusal::DroppedNamespace { namespace: Arc::from("test.b") })
    );
}

// Catches: a republish of the same bytes staged as a new publication rather
// than the no-op §9 makes it.
#[test]
fn republishing_the_same_hash_changes_nothing() {
    let module = surface(&[("test.a", contract(&[KEPT], false))], &[]);

    assert_eq!(admit_over(&[(hash(1), &module)], hash(1), &module), Ok(Admitted::Unchanged));
    assert_eq!(admit_over(&[], hash(1), &module), Ok(Admitted::Publish));
}

/// A registry with its owner attached, and the module cache publishes check
/// modules in through.
struct Fixture {
    registry: Arc<Registry>,
    owner: RegistryOwnerLease,
    modules: ModuleCache,
    blobs: BlobCheckIn,
}

impl Fixture {
    fn new() -> Self {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let owner = RegistryOwnerLease::attach(
            auth(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let blobs = BlobCheckIn::new(BlobStore::new().expect("spawn the reclaim thread"));
        Self { registry, owner, modules: ModuleCache::new(Arc::new(Engine::default())), blobs }
    }

    /// A module exporting each `(namespace, rows)` group, every row replying
    /// nothing.
    fn module(&self, groups: &[(&str, &[KindId])]) -> Module {
        self.build(groups, "")
    }

    /// [`Self::module`] carrying the content-addressed marker.
    fn content_addressed_module(&self, groups: &[(&str, &[KindId])]) -> Module {
        self.build(groups, &format!(r#"(@custom "{CONTENT_ADDRESSED_SECTION}" "\01")"#))
    }

    fn build(&self, groups: &[(&str, &[KindId])], marker: &str) -> Module {
        let exported = inputs_section(groups.iter().map(|(namespace, rows)| (*namespace, *rows, false)));
        let wat = format!(r#"(module {} {marker} (func (export "noop")))"#, custom(INPUTS_SECTION, &exported));
        self.check_in(&wat)
    }

    /// A module exporting each `(namespace, rows)` group and declaring each
    /// `(namespace, instanced)` private group, with `lineage` as its
    /// placement records.
    fn placed_module(
        &self,
        exported: &[(&str, &[KindId])],
        private: &[(&str, bool)],
        lineage: &[ActorLineageRecord],
    ) -> Module {
        let exported = inputs_section(exported.iter().map(|(namespace, rows)| (*namespace, *rows, false)));
        let private = inputs_section(private.iter().map(|(namespace, instanced)| (*namespace, &[][..], *instanced)));
        let lineage = lineage.iter().fold(Vec::new(), |mut section, record| {
            section.push(ACTOR_LINEAGE_SECTION_VERSION);
            section.extend(wire::to_vec(record).expect("encode a lineage record"));
            section
        });
        let wat = format!(
            r#"(module {} {} {} (func (export "noop")))"#,
            custom(INPUTS_SECTION, &exported),
            custom(PRIVATE_INPUTS_SECTION, &private),
            custom(ACTOR_LINEAGE_SECTION, &lineage),
        );
        self.check_in(&wat)
    }

    fn check_in(&self, wat: &str) -> Module {
        let code = Blob::from(wat::parse_str(wat).expect("parse the fixture WAT"));
        self.modules.check_in(&self.blobs, &code).expect("check the module in")
    }

    fn apply(&self, batch: EffectBatch) -> Result<Vec<RegistryApplied>, RegistryEffectError> {
        let completion = self.registry.submit(batch).expect("the attached owner reserves the batch");
        self.owner.run_once();
        completion.wait_timeout(Duration::from_secs(5)).expect("the owner completes the batch")
    }

    fn publish(&self, module: &Module) -> Result<(), String> {
        self.apply(RegistryBatch::publish_module(module).into_effects()).map(drop).map_err(|error| error.to_string())
    }
}

/// One inputs section of boundary-led groups, each `(namespace, rows,
/// instanced)` group's handlers replying nothing.
fn inputs_section<'a>(groups: impl Iterator<Item = (&'a str, &'a [KindId], bool)>) -> Vec<u8> {
    let mut section = Vec::new();
    for (namespace, rows, instanced) in groups {
        let boundary = InputsRecord::ActorBoundary { namespace: namespace.to_owned().into() };
        let handlers = rows.iter().map(|id| InputsRecord::Handler {
            id: *id,
            name: id.to_string().into(),
            doc: None,
            reply: ReplyContract::None,
        });
        let cardinality = instanced.then_some(InputsRecord::Instanced);
        for record in iter::once(boundary).chain(handlers).chain(cardinality) {
            section.push(INPUTS_SECTION_VERSION);
            section.extend(wire::to_vec(&record).expect("encode an inputs record"));
        }
    }
    section
}

/// A WAT custom section named `name` holding `bytes`.
fn custom(name: &str, bytes: &[u8]) -> String {
    let escaped = bytes.iter().fold(String::new(), |mut escaped, byte| {
        write!(escaped, "\\{byte:02x}").expect("write to a String");
        escaped
    });
    format!(r#"(@custom "{name}" "{escaped}")"#)
}

// Catches: admission comparing against the first publication of a namespace
// rather than its current one, so rows and namespaces a republish added can
// later be dropped.
#[test]
fn a_republish_binds_the_next_to_what_it_added() {
    let fixture = Fixture::new();

    fixture.publish(&fixture.module(&[("test.x", &[KEPT])])).expect("first publish");
    fixture.publish(&fixture.module(&[("test.x", &[KEPT, ADDED]), ("test.y", &[KEPT])])).expect("growing republish");

    let narrowed = fixture.publish(&fixture.module(&[("test.x", &[KEPT]), ("test.y", &[KEPT])]));
    assert!(narrowed.as_ref().is_err_and(|error| error.starts_with("test.x drops or changes its row")), "{narrowed:?}");
    let dropped = fixture.publish(&fixture.module(&[("test.x", &[KEPT, ADDED])]));
    assert!(dropped.as_ref().is_err_and(|error| error.starts_with("test.y is exported")), "{dropped:?}");
}

// Catches: the staged table committed by a batch that fails after its
// publish effect, so a refused load or replace still binds its namespaces.
#[test]
fn a_failed_batch_publishes_nothing() {
    let fixture = Fixture::new();
    let batch = EffectBatch::new(vec![
        RegistryEffect::PublishModule(fixture.module(&[("test.x", &[KEPT, ADDED])])),
        RegistryEffect::DropMailbox(canonical_mailbox_id("test.publication.absent")),
    ]);

    assert!(matches!(fixture.apply(batch), Err(RegistryEffectError::Drop(_))));
    fixture.publish(&fixture.module(&[("test.x", &[KEPT])])).expect("test.x has no predecessor to narrow");
}

// Catches: a content-addressed export published under its declared namespace,
// which refuses a unit's second bundle as a republish that drops a row (the
// PR #6885 CI failure), or qualified by the wrong or a truncated hash.
#[test]
fn every_content_addressed_build_is_its_own_publication() {
    const BUNDLE: &str = "test.publication.bundle";
    let fixture = Fixture::new();
    let first = fixture.content_addressed_module(&[(BUNDLE, &[KEPT, ADDED])]);
    let second = fixture.content_addressed_module(&[(BUNDLE, &[KEPT])]);

    let hex = first.hash().as_bytes().iter().fold(String::new(), |mut hex, byte| {
        write!(hex, "{byte:02x}").expect("write to a String");
        hex
    });
    let surface = ModuleSurface::of(&first);
    let published: Vec<_> = surface.exported_namespaces().map(|namespace| &**namespace).collect();
    assert_eq!(published, [format!("{BUNDLE}.{hex}").as_str()]);

    fixture.publish(&first).expect("the first build publishes");
    fixture.publish(&first).expect("a second unit on the same build shares its publication");
    fixture.publish(&second).expect("a build that drops a row is not the first build's successor");
    fixture.publish(&fixture.module(&[(BUNDLE, &[KEPT])])).expect("no build publishes the bare declared namespace");
}

/// The guest a load adopts: its namespace is published by a module.
struct Guest;

impl Addressable for Guest {
    const NAMESPACE: &'static str = "test.publication.guest";
    type Resolver = One;
}

impl Fixture {
    /// Register a live route at `name`'s lineage position.
    fn register(&self, name: &str) {
        self.registry
            .try_register_inbox_with_id(&auth(), lineage_mailbox_id(name), name, noop_handler())
            .expect("register the route");
    }

    fn category(&self, name: &str) -> Option<MailboxCategory> {
        let descriptors = self.registry.list_mailbox_descriptors();
        descriptors.iter().find(|descriptor| descriptor.name == name).expect("the route is listed").category
    }
}

// Catches: a guest categorised by its name's spelling rather than the
// publication table, so a guest named by its own namespace is lost from
// `ListComponents`, or a route no module implements is listed as one
// (ADR-0241 §3).
#[test]
fn a_route_is_a_guest_where_a_published_module_holds_its_leaf_namespace() {
    let fixture = Fixture::new();
    fixture.publish(&fixture.module(&[(Guest::NAMESPACE, &[KEPT])])).expect("publish the guest's module");

    let guests = [Guest::NAMESPACE, "test.publication.guest:k", "test.publication.host/test.publication.guest:k"];
    for name in guests.into_iter().chain(["test.publication.unpublished"]) {
        fixture.register(name);
    }

    for name in guests {
        assert_eq!(fixture.category(name), Some(MailboxCategory::Trampoline), "{name}");
    }
    assert_eq!(fixture.category("test.publication.unpublished"), None);
}

// Catches: a typed adoption over an arbitrary live actor, which would hand an
// embedder an `ActorRef<R>` for something that never loaded as `R`; the
// refusal keeps `adopt_load` a load door rather than a generic mint.
#[test]
fn loaded_adopts_only_a_published_guest() {
    let fixture = Fixture::new();
    fixture.publish(&fixture.module(&[(Guest::NAMESPACE, &[KEPT])])).expect("publish the guest's module");
    fixture.register(Guest::NAMESPACE);
    fixture.register("test.publication.not_a_guest");

    let resolve = |name: &str| fixture.registry.resolve_live(lineage_mailbox_id(name)).expect("the route is live");

    assert!(fixture.registry.loaded::<Guest>(resolve(Guest::NAMESPACE)).is_ok());
    assert_eq!(
        fixture.registry.loaded::<Guest>(resolve("test.publication.not_a_guest")),
        Err(AdoptRefused::NotComponent)
    );
}

// Catches: an address index built once from link-time facts, so a hole
// beneath a published guest never fills; a module whose private group
// redeclares a native namespace switching that native short path off as
// contradictory; and a module's malformed namespace failing the whole index.
#[test]
fn a_publish_extends_the_short_path_index_without_touching_native_paths() {
    const PARENT: &str = "test.publication.guest_parent";
    const CHILD: &str = "test.publication.guest_child";
    const MALFORMED: &str = "test.publication.Malformed Namespace";
    let fixture = Fixture::new();
    let native = format!("{NATIVE_ROOT}/{NATIVE_INSTANCED}:k");
    let guest = format!("{PARENT}/{CHILD}:k");
    fixture.register(&native);
    fixture.register(&guest);
    let expand = |short: &str| {
        let path = ErasedActorPath::new(short).expect("a well-formed short path");
        fixture.registry.resolve_address(&path).map(|resolved| resolved.canonical_path)
    };

    assert_eq!(expand(&format!("{PARENT}/:k")), Err(AddressResolutionError::UnknownRoot { root: PARENT.to_owned() }));

    let child = |parent: &str, child: &str| ActorLineageRecord::Child {
        parent: ActorId::singleton(parent).0,
        child: ActorId::singleton(child).0,
        parent_namespace: parent.to_owned().into(),
        child_namespace: child.to_owned().into(),
    };
    let module = fixture.placed_module(
        &[(PARENT, &[KEPT])],
        &[(CHILD, true), (NATIVE_INSTANCED, false), (MALFORMED, true)],
        &[
            ActorLineageRecord::Root { actor: ActorId::singleton(PARENT).0, namespace: PARENT.into() },
            child(PARENT, CHILD),
            child(PARENT, MALFORMED),
        ],
    );
    fixture.publish(&module).expect("publish the guest module");

    assert_eq!(expand(&format!("{PARENT}/:k")), Ok(guest));
    assert_eq!(expand(&format!("{NATIVE_ROOT}/:k")), Ok(native), "a module cannot switch off a native short path");
}
