//! Tests for the publication table and its admission step (ADR-0241 §3/§4).
//!
//! The rule tests run [`admit`] over hand-built surfaces, so each names one
//! rule without wasm. The native test reads the real link-time inventory, and
//! the registry tests publish checked-in WAT modules through the owner, so
//! the table they exercise is the one a load stages.

use std::fmt::Write as _;
use std::iter;
use std::sync::Arc;
use std::time::Duration;

use aether_data::name_inventory::{NameEntry, ParamKind, TemplateEntry, inventory};
use aether_data::{
    Blob, BlobHash, INPUTS_SECTION, INPUTS_SECTION_VERSION, InputsRecord, MAILBOX_DOMAIN, ReplyContract, THREAD_DOMAIN,
    wire,
};
use aether_kinds::{ComponentCapabilities, FallbackCapability, HandlerCapability};
use wasmtime::Engine;

use super::{AdmissionRefusal, Admitted, ModuleSurface, PublicationTable, admit};
use crate::actor::native::BlobCheckIn;
use crate::actor::wasm::module::{Module, ModuleCache};
use crate::config::RegistryQueueCapacities;
use crate::mail::KindId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{EffectBatch, RegistryApplied, RegistryBatch, RegistryEffect, RegistryEffectError};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{ContractBreak, Registry, RouteContract, canonical_mailbox_id};
use crate::scheduler::WakeSink;
use crate::store::BlobStore;
use crate::testing::boot_authority as auth;

const NATIVE_SINGLETON: &str = "test.publication.native_singleton";
const NATIVE_INSTANCED: &str = "test.publication.native_instanced";
const THREAD_NAMED: &str = "test.publication.thread_named";
const OTHER_TEMPLATE: &str = "test.publication.other_template";

inventory::submit! { NameEntry { domain: MAILBOX_DOMAIN, name: NATIVE_SINGLETON } }
inventory::submit! {
    TemplateEntry { domain: MAILBOX_DOMAIN, prefix: NATIVE_INSTANCED, template: ":{subname}", param: ParamKind::Dynamic }
}
inventory::submit! { NameEntry { domain: THREAD_DOMAIN, name: THREAD_NAMED } }
inventory::submit! {
    TemplateEntry { domain: MAILBOX_DOMAIN, prefix: OTHER_TEMPLATE, template: "-{subname}", param: ParamKind::Dynamic }
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
    let table = PublicationTable::from_inventory();
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
        let mut section = Vec::new();
        for (namespace, rows) in groups {
            let boundary = InputsRecord::ActorBoundary { namespace: (*namespace).to_owned().into() };
            let handlers = rows.iter().map(|id| InputsRecord::Handler {
                id: *id,
                name: id.to_string().into(),
                doc: None,
                reply: ReplyContract::None,
            });
            for record in iter::once(boundary).chain(handlers) {
                section.push(INPUTS_SECTION_VERSION);
                section.extend(wire::to_vec(&record).expect("encode an inputs record"));
            }
        }
        let escaped = section.iter().fold(String::new(), |mut escaped, byte| {
            write!(escaped, "\\{byte:02x}").expect("write to a String");
            escaped
        });
        let wat = format!(r#"(module (@custom "{INPUTS_SECTION}" "{escaped}") (func (export "noop")))"#);
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
