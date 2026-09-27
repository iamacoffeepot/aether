//! Tests for [`super::super::mailbox::resolve`] — the name lookup walk
//! and the structured misses it reports.

use std::sync::Arc;
use std::time::Duration;

use aether_actor::{Protocol, ProtocolPath, ResolveError, Row, RowSet};
use aether_data::tagged_id::{Tag, with_tag};
use aether_data::wire::{self, DecodeCtx, PublishedRoutes};
use aether_data::{ActorId, ErasedActorPath, Kind, MAILBOX_DOMAIN, ReplyContract, fnv1a_64_prefixed, fold_lineage};
use aether_kinds::{ComponentCapabilities, HandlerCapability};

use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{EffectBatch, RegistryEffect};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{
    AddressResolutionError, Registry, RouteContract, canonical_mailbox_id, lineage_mailbox_id, noop_handler,
};
use crate::mail::{KindId, MailboxId};
use crate::scheduler::WakeSink;
use crate::testing::boot_authority as auth;

use super::support::starting_token;

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

/// One single row: the protocol a received path claims.
struct Loading;

impl Protocol for Loading {
    type Rows = (Row<Load, Loaded>,);
}

#[aether_data::kind(name = "test.resolve_protocol.carries", no_serde)]
struct Carries {
    path: ProtocolPath<Loading>,
}

/// Answers `Loading`'s rows for every path, so a decode proves any path:
/// `resolve` is what these tests exercise, not the decode's proof.
struct CoversLoading;

impl PublishedRoutes for CoversLoading {
    fn published_rows(&self, _path: &ErasedActorPath) -> Option<Arc<[(KindId, ReplyContract)]>> {
        Some(<<Loading as Protocol>::Rows as RowSet>::CONTRACTS.into())
    }
}

/// A protocol path as a native actor receives one: decoded from wire bytes
/// against a context whose routes cover `Loading`.
fn received(text: &str) -> ProtocolPath<Loading> {
    let bytes = wire::encode_to_vec(&path(text)).expect("a path encodes");

    Carries::decode_with(&bytes, &mut DecodeCtx::empty().routes(&CoversLoading)).expect("a canonical path decodes").path
}

fn path(text: &str) -> ErasedActorPath {
    ErasedActorPath::new(text).expect("fixture is an actor path")
}

/// ADR-0231 §3's receipt of a protocol path proves liveness at the path's
/// canonical name. Each case names the bug it catches:
///
/// - a `Live` closure route that publishes no rows mints: a resolve that
///   still reads or requires the route's published rows;
/// - a `Starting` reservation and a dropped route refuse `NotLive`: a
///   liveness read that accepts any recorded route;
/// - a route at the path's fold under another canonical name refuses
///   `NotLive`: a missing name check at a fold collision.
#[test]
fn resolve_protocol_proves_live_routes_and_refuses_the_rest() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );

    let live = "test.resolve_protocol.live";
    registry.try_register_inbox(&auth(), live, noop_handler()).expect("the route name is free");
    registry.resolve_protocol(&received(live)).expect("a live route mints, with no rows published");

    let starting = "test.resolve_protocol.starting";
    let reserved =
        registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(starting.to_owned())])).expect("submits");
    owner.run_once();
    starting_token(
        &reserved.wait_timeout(Duration::from_millis(100)).expect("reservation completes").expect("reserves"),
    );
    let dropped = "test.resolve_protocol.dropped";
    let dropped_id = registry.try_register_inbox(&auth(), dropped, noop_handler()).expect("the route name is free");
    registry.drop_mailbox(&auth(), dropped_id).expect("the live route retires");
    for name in [starting, dropped] {
        assert_eq!(
            registry.resolve_protocol(&received(name)).expect_err("no live route"),
            ResolveError::NotLive { path: path(name) },
            "{name}",
        );
    }

    let folded = "test.resolve_protocol.folded";
    registry
        .try_register_inbox_with_id(
            &auth(),
            lineage_mailbox_id(folded),
            "test.resolve_protocol.impostor",
            noop_handler(),
        )
        .expect("the fold is free");
    assert_eq!(
        registry.resolve_protocol(&received(folded)).expect_err("another name stands at the fold"),
        ResolveError::NotLive { path: path(folded) },
    );
}

/// A route contract publishing `rows`, built the way both transports build
/// theirs: from a receive surface.
fn contract(rows: &[(KindId, ReplyContract)]) -> RouteContract {
    RouteContract::from_capabilities(&ComponentCapabilities {
        handlers: rows
            .iter()
            .map(|(id, reply)| HandlerCapability { id: *id, name: String::new(), doc: None, reply: *reply })
            .collect(),
        ..ComponentCapabilities::default()
    })
}

/// The registry's `PublishedRoutes` answer, which a `ProtocolPath` decode
/// checks coverage against, reads only the `Live` route standing under
/// exactly the path. Each case names the bug it catches:
///
/// - a `Live` route answers exactly its published rows, and a closure route
///   its empty rows: an answer built from anything but the published
///   contract;
/// - a `Starting` reservation and a dropped route answer `None`: a read of a
///   route that is not `Live`;
/// - a route at the path's fold under another canonical name answers `None`:
///   a `published_contract` read at the fold without `live_route`'s name
///   check, which answers another route's rows.
#[test]
fn published_rows_answer_only_the_live_route_under_the_path() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let rows = [(Load::ID, ReplyContract::One(Loaded::ID))];

    let live = "test.published_rows.live";
    let live_id = registry.try_register_inbox(&auth(), live, noop_handler()).expect("the route name is free");
    registry.publish_contract(&auth(), live_id, contract(&rows)).expect("an empty contract takes any rows");

    let closure = "test.published_rows.closure";
    registry.try_register_inbox(&auth(), closure, noop_handler()).expect("the route name is free");

    assert_eq!(registry.published_rows(&path(live)).as_deref(), Some(&rows[..]));
    assert_eq!(registry.published_rows(&path(closure)).as_deref(), Some(&[][..]));

    let starting = "test.published_rows.starting";
    let reserved =
        registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(starting.to_owned())])).expect("submits");
    owner.run_once();
    starting_token(
        &reserved.wait_timeout(Duration::from_millis(100)).expect("reservation completes").expect("reserves"),
    );

    let dropped = "test.published_rows.dropped";
    let dropped_id = registry.try_register_inbox(&auth(), dropped, noop_handler()).expect("the route name is free");
    registry.publish_contract(&auth(), dropped_id, contract(&rows)).expect("an empty contract takes any rows");
    registry.drop_mailbox(&auth(), dropped_id).expect("the live route retires");

    let folded = "test.published_rows.folded";
    let impostor = registry
        .try_register_inbox_with_id(&auth(), lineage_mailbox_id(folded), "test.published_rows.impostor", noop_handler())
        .expect("the fold is free");
    registry.publish_contract(&auth(), impostor, contract(&rows)).expect("an empty contract takes any rows");

    for name in [starting, dropped, folded] {
        assert_eq!(registry.published_rows(&path(name)), None, "{name}");
    }
}
