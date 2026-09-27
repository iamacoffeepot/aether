//! Tests for [`super::super::mailbox::resolve`] — the name lookup walk
//! and the structured misses it reports.

use std::sync::Arc;
use std::time::Duration;

use aether_actor::{Protocol, ProtocolPath, ResolveError, Row, Silent};
use aether_data::tagged_id::{Tag, with_tag};
use aether_data::wire::{WireEncode, decode_from_slice};
use aether_data::{ActorId, ErasedActorPath, Kind, MAILBOX_DOMAIN, ReplyContract, fnv1a_64_prefixed, fold_lineage};

use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{EffectBatch, RegistryEffect};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{AddressResolutionError, Registry, canonical_mailbox_id, lineage_mailbox_id, noop_handler};
use crate::mail::{KindId, MailboxId};
use crate::scheduler::WakeSink;
use crate::testing::boot_authority as auth;

use super::support::{contract, starting_token};

#[test]
fn lookup_missing_returns_none() {
    let r = Registry::new();
    assert!(r.lookup("nope").is_none());
    assert!(r.entry_at(MailboxId(42)).is_none());
}

#[test]
fn lookup_over_depth_scope_path_is_resolution_miss() {
    let r = Registry::new();
    // One segment past `MAX_SCOPE_PATH_DEPTH`: rejected before the fold.
    let name = (0..=aether_data::MAX_SCOPE_PATH_DEPTH).map(|i| format!("seg{i}")).collect::<Vec<_>>().join("/");
    assert!(r.lookup(&name).is_none());
}

#[test]
fn lookup_over_bytes_scope_path_is_resolution_miss() {
    let r = Registry::new();
    // Single segment longer than the byte cap (depth stays 1).
    let name = "a".repeat(aether_data::MAX_SCOPE_PATH_BYTES + 1);
    assert!(r.lookup(&name).is_none());
}

#[test]
fn canonical_resolution_reports_the_registered_path_and_structured_misses() {
    let r = Registry::new();
    let canonical = "root/worker:camera";
    let id = lineage_mailbox_id(canonical);
    r.try_register_inbox_with_id(&auth(), id, canonical, noop_handler()).unwrap();

    let path = |text| ErasedActorPath::new(text).expect("fixture is a well-formed actor path");
    let resolved = r.resolve_address(&path(canonical)).expect("canonical mailbox is live");
    assert_eq!(resolved.mailbox_id, id);
    assert_eq!(resolved.canonical_path, canonical);
    assert_eq!(
        r.resolve_address(&path("root/worker:missing")),
        Err(AddressResolutionError::NoLiveMailbox { canonical_path: "root/worker:missing".to_owned() })
    );
}

#[test]
fn lineage_fold_is_the_node_chain_and_meets_the_canonical_id_at_depth_one() {
    // Tripwire: lookup by path meets registration by name only while the
    // depth-1 fold equals the id a by-name registration takes, and a nested
    // path must fold node by node rather than hash the joined string.
    for name in ["aether.component", "aether.embedded:camera"] {
        assert_eq!(lineage_mailbox_id(name).0, canonical_mailbox_id(name).0, "{name}");
    }

    let path = "root/scope:7/leaf";
    let chain = fold_lineage(
        fold_lineage(ActorId::singleton("root").0, ActorId::instanced("scope", "7")),
        ActorId::singleton("leaf"),
    );
    assert_eq!(lineage_mailbox_id(path).0, with_tag(Tag::Mailbox, chain));
    assert_ne!(lineage_mailbox_id(path).0, with_tag(Tag::Mailbox, fnv1a_64_prefixed(MAILBOX_DOMAIN, path.as_bytes())));
}

#[test]
fn mailbox_name_reverse_lookup() {
    let r = Registry::new();
    let a = r.register_inbox(&auth(), "physics", noop_handler());
    let b = r.register_inbox(&auth(), "graphics", noop_handler());
    assert_eq!(r.mailbox_name(a).as_deref(), Some("physics"));
    assert_eq!(r.mailbox_name(b).as_deref(), Some("graphics"));
    assert!(r.mailbox_name(MailboxId(999)).is_none());
}

#[aether_data::kind(name = "test.resolve_protocol.load", copy)]
struct Load {
    seq: u32,
}

#[aether_data::kind(name = "test.resolve_protocol.loaded", copy)]
struct Loaded {
    seq: u32,
}

#[aether_data::kind(name = "test.resolve_protocol.set_mode", copy)]
struct SetMode {
    mode: u32,
}

#[aether_data::kind(name = "test.resolve_protocol.probe", copy)]
struct Probe {
    seq: u32,
}

/// One single row and one silent row.
struct Covered;

impl Protocol for Covered {
    type Rows = (Row<Load, Loaded>, Row<SetMode, Silent>);
}

/// [`Covered`]'s rows plus a silent [`Probe`] row.
struct Wider;

impl Protocol for Wider {
    type Rows = (Row<Load, Loaded>, Row<SetMode, Silent>, Row<Probe, Silent>);
}

/// A protocol path as a native actor receives one: decoded from wire bytes,
/// carrying only its writer's claim.
fn received<P>(text: &str) -> ProtocolPath<P> {
    let mut wire = Vec::new();
    ErasedActorPath::new(text).expect("fixture is an actor path").encode(&mut wire).expect("a path encodes");
    decode_from_slice(&wire).expect("a canonical path decodes")
}

fn path(text: &str) -> ErasedActorPath {
    ErasedActorPath::new(text).expect("fixture is an actor path")
}

/// A live closure route under `name` publishing `rows`.
fn published(registry: &Registry, name: &str, rows: &[(KindId, ReplyContract)]) -> MailboxId {
    let id = registry.try_register_inbox(&auth(), name, noop_handler()).expect("the route name is free");
    registry.publish_contract(&auth(), id, contract(rows, false)).expect("an empty contract takes any rows");
    id
}

/// ADR-0231 §3's receipt of a decoded protocol path. Each case names the bug
/// it catches:
///
/// - a superset of `Covered`'s rows mints: a check that demands equal row
///   sets;
/// - a single row replying another kind, or published `Manual`, refuses
///   naming that kind: a check by kind alone, or a `Manual` row counted as
///   covering;
/// - `Wider` refuses after `Covered` minted on the same route: a coverage
///   cache keyed by route alone;
/// - a `Starting` reservation and a dropped route refuse `NotLive`: a
///   liveness read that accepts any recorded route;
/// - a route at the path's fold under another canonical name refuses
///   `NotLive`: a missing name check at a fold collision.
#[test]
fn resolve_protocol_proves_live_covered_routes_and_names_the_rest() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let covering = [(Load::ID, ReplyContract::One(Loaded::ID)), (SetMode::ID, ReplyContract::None)];

    let superset = "test.resolve_protocol.superset";
    published(
        &registry,
        superset,
        &[
            (Load::ID, ReplyContract::One(Loaded::ID)),
            (SetMode::ID, ReplyContract::None),
            (Probe::ID, ReplyContract::One(Loaded::ID)),
        ],
    );
    registry.resolve_protocol(&received::<Covered>(superset)).expect("a superset of the rows covers the protocol");
    assert_eq!(
        registry.resolve_protocol(&received::<Wider>(superset)).expect_err("the probe row replies"),
        ResolveError::Uncovered { path: path(superset), kind: Probe::ID },
        "a covered answer is kept per protocol, not per route",
    );

    let other_reply = "test.resolve_protocol.other_reply";
    published(
        &registry,
        other_reply,
        &[(Load::ID, ReplyContract::One(SetMode::ID)), (SetMode::ID, ReplyContract::None)],
    );
    assert_eq!(
        registry.resolve_protocol(&received::<Covered>(other_reply)).expect_err("the load row replies another kind"),
        ResolveError::Uncovered { path: path(other_reply), kind: Load::ID },
    );
    let manual = "test.resolve_protocol.manual";
    published(&registry, manual, &[(Load::ID, ReplyContract::Manual), (SetMode::ID, ReplyContract::None)]);
    assert_eq!(
        registry.resolve_protocol(&received::<Covered>(manual)).expect_err("a manual row covers nothing"),
        ResolveError::Uncovered { path: path(manual), kind: Load::ID },
    );

    let starting = "test.resolve_protocol.starting";
    let reserved =
        registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(starting.to_owned())])).expect("submits");
    owner.run_once();
    starting_token(
        &reserved.wait_timeout(Duration::from_millis(100)).expect("reservation completes").expect("reserves"),
    );
    let dropped = "test.resolve_protocol.dropped";
    let dropped_id = published(&registry, dropped, &covering);
    registry.drop_mailbox(&auth(), dropped_id).expect("the live route retires");
    for name in [starting, dropped] {
        assert_eq!(
            registry.resolve_protocol(&received::<Covered>(name)).expect_err("no live route"),
            ResolveError::NotLive { path: path(name) },
            "{name}",
        );
    }

    let folded = "test.resolve_protocol.folded";
    let impostor = registry
        .try_register_inbox_with_id(
            &auth(),
            lineage_mailbox_id(folded),
            "test.resolve_protocol.impostor",
            noop_handler(),
        )
        .expect("the fold is free");
    registry.publish_contract(&auth(), impostor, contract(&covering, false)).expect("an empty contract takes any rows");
    assert_eq!(
        registry.resolve_protocol(&received::<Covered>(folded)).expect_err("another name stands at the fold"),
        ResolveError::NotLive { path: path(folded) },
    );
}
