//! Tests for [`super::super::mailbox::resolve`] — the name lookup walk
//! and the structured misses it reports.

use std::sync::Arc;

use aether_actor::__macro_internals::ProtocolCast;
use aether_actor::{Protocol, ProtocolPath, ResolveError, Row, RowSet, Undeclared};
use aether_data::tagged_id::{Tag, with_tag};
use aether_data::wire::{self, DecodeCtx, PublishedRoutes};
use aether_data::{ActorId, ErasedActorPath, Kind, MAILBOX_DOMAIN, ReplyContract, fnv1a_64_prefixed, fold_lineage};
use aether_kinds::{ComponentCapabilities, FallbackCapability, HandlerCapability};

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
    r.register_inbox(&auth(), "root", noop_handler());
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
    for name in ["aether.component", "aether.kit.camera:main"] {
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

/// The ordinary protocol cast arm for an unchecked row, distinct from the
/// subscriber exception exercised below.
struct UncheckedLoading;

impl Protocol for UncheckedLoading {
    type Rows = (Row<Load, Undeclared>,);
}

impl ProtocolCast for UncheckedLoading {}

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
    starting_token(&reserved.try_take().expect("reservation completes").expect("reserves"));
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
pub(super) fn contract(rows: &[(KindId, ReplyContract)]) -> RouteContract {
    RouteContract::from_capabilities(&ComponentCapabilities {
        handlers: rows
            .iter()
            .map(|(id, reply)| HandlerCapability {
                id: *id,
                name: String::new(),
                doc: None,
                reply: *reply,
                reason: None,
            })
            .collect(),
        ..ComponentCapabilities::default()
    })
}

/// A route contract with only `#[fallback]`, and therefore no explicit row.
fn fallback_contract() -> RouteContract {
    RouteContract::from_capabilities(&ComponentCapabilities {
        fallback: Some(FallbackCapability { doc: None }),
        ..ComponentCapabilities::default()
    })
}

/// The registry's `PublishedRoutes` answer, which a `ProtocolPath` decode
/// checks coverage against, reads the `Live` or `Dropped` route standing
/// under exactly the path. Each case names the bug it catches:
///
/// - a `Live` route answers exactly its published rows, and a closure route
///   its empty rows: an answer built from anything but the published
///   contract;
/// - a dropped route answers the rows it last published while
///   `published_contract` still reads `None`: a retire that discards the
///   contract, so a closed actor's path is refused at decode and its
///   handler never answers, or a live-only read widened to dead senders;
/// - a never-registered path, a `Starting` reservation, and a route at the
///   path's fold under another canonical name answer `None`: a read that
///   proves a path no route has stood at, or skips `live_route`'s name
///   check and answers another route's rows;
/// - a `Carries` payload decodes over the registry for the dropped path and
///   is refused for the never-registered one: the decode reading anything
///   but these rows.
#[test]
fn route_rows_answer_live_and_dropped_routes_under_the_path() {
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

    let live = "test.route_rows.live";
    let live_id = registry.try_register_inbox(&auth(), live, noop_handler()).expect("the route name is free");
    registry.publish_contract(&auth(), live_id, contract(&rows)).expect("an empty contract takes any rows");

    let closure = "test.route_rows.closure";
    registry.try_register_inbox(&auth(), closure, noop_handler()).expect("the route name is free");

    assert_eq!(registry.published_rows(&path(live)).as_deref(), Some(&rows[..]));
    assert_eq!(registry.published_rows(&path(closure)).as_deref(), Some(&[][..]));

    let dropped = "test.route_rows.dropped";
    let dropped_id = registry.try_register_inbox(&auth(), dropped, noop_handler()).expect("the route name is free");
    registry.publish_contract(&auth(), dropped_id, contract(&rows)).expect("an empty contract takes any rows");
    registry.drop_mailbox(&auth(), dropped_id).expect("the live route retires");

    assert_eq!(registry.published_rows(&path(dropped)).as_deref(), Some(&rows[..]));
    assert!(registry.published_contract(dropped_id).is_none(), "a dropped route publishes no live contract");

    let starting = "test.route_rows.starting";
    let reserved =
        registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(starting.to_owned())])).expect("submits");
    owner.run_once();
    starting_token(&reserved.try_take().expect("reservation completes").expect("reserves"));

    let folded = "test.route_rows.folded";
    let impostor = registry
        .try_register_inbox_with_id(&auth(), lineage_mailbox_id(folded), "test.route_rows.impostor", noop_handler())
        .expect("the fold is free");
    registry.publish_contract(&auth(), impostor, contract(&rows)).expect("an empty contract takes any rows");

    let never = "test.route_rows.never";
    for name in [never, starting, folded] {
        assert_eq!(registry.published_rows(&path(name)), None, "{name}");
    }

    let decode = |name: &str| {
        let bytes = wire::encode_to_vec(&path(name)).expect("a path encodes");
        Carries::decode_with(&bytes, &mut DecodeCtx::empty().routes(&*registry)).map(|carries| carries.path)
    };
    assert_eq!(decode(dropped).expect("a dropped route's path proves its type").as_erased(), &path(dropped),);
    decode(never).expect_err("a path no route has stood at is refused");
}

/// ADR-0231 §4's guard cast types a held reference as `Subscriber<Load>`
/// only while its route is `Live` and publishes a silent or unchecked `Load`
/// row, and as `UncheckedLoading` only for the exact unchecked row. Each case names
/// the bug it catches:
///
/// - a `Live` route publishing `(Load, None)` or `(Load, Unchecked)` mints: a
///   cast that reads anything but the published rows, or that refuses the
///   unchecked row a subscriber may answer an event with;
/// - the ordinary unchecked protocol admits `(Load, Unchecked)` but refuses
///   `(Load, None)`, `(Load, One(Loaded))`, a missing row, and a fallback-only
///   route: a cast that treats unchecked as a
///   wildcard or applies the subscriber exception to every protocol;
/// - a `Starting` reservation and a dropped route answer `None`: a cast that
///   mints for a sender that is not live.
#[test]
fn cast_mints_only_for_a_live_route_whose_rows_the_protocol_admits() {
    use aether_actor::Subscriber;

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let stand = |name: &str, rows: &[(KindId, ReplyContract)]| {
        let id = registry.try_register_inbox(&auth(), name, noop_handler()).expect("the route name is free");
        registry.publish_contract(&auth(), id, contract(rows)).expect("an empty contract takes any rows");
        registry.resolve_live(id).expect("the route is live")
    };
    let cast = |reference| registry.cast::<Subscriber<Load>>(reference).is_some();
    let cast_unchecked = |reference| registry.cast::<UncheckedLoading>(reference).is_some();

    let silent = stand("test.cast.silent", &[(Load::ID, ReplyContract::None)]);
    let unchecked =
        stand("test.cast.unchecked", &[(Loaded::ID, ReplyContract::None), (Load::ID, ReplyContract::Unchecked)]);
    assert!(cast(silent), "a silent row answers the subscriber protocol");
    assert!(cast(unchecked), "an unchecked row answers it too");
    assert!(cast_unchecked(unchecked), "the exact unchecked row answers an ordinary unchecked protocol");
    assert!(!cast_unchecked(silent), "a silent row does not answer an unchecked protocol");

    let replying = stand("test.cast.replying", &[(Load::ID, ReplyContract::One(Loaded::ID))]);
    let unrelated = stand("test.cast.unrelated", &[(Loaded::ID, ReplyContract::None)]);
    let closure = stand("test.cast.closure", &[]);
    let fallback_id =
        registry.try_register_inbox(&auth(), "test.cast.fallback", noop_handler()).expect("the route name is free");
    registry.publish_contract(&auth(), fallback_id, fallback_contract()).expect("an empty contract takes fallback");
    let fallback = registry.resolve_live(fallback_id).expect("the route is live");
    for (name, reference) in
        [("replying", replying), ("unrelated", unrelated), ("closure", closure), ("fallback", fallback)]
    {
        assert!(!cast(reference), "{name}: the published rows do not answer the protocol");
        assert!(!cast_unchecked(reference), "{name}: the published rows do not answer the unchecked protocol");
    }

    let starting = "test.cast.starting";
    let reserved =
        registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(starting.to_owned())])).expect("submits");
    owner.run_once();
    starting_token(&reserved.try_take().expect("reservation completes").expect("reserves"));
    let starting = registry.stamped_sender(lineage_mailbox_id(starting)).expect("a Starting record stands there");
    let dropped = stand("test.cast.dropped", &[(Load::ID, ReplyContract::None)]);
    registry.drop_mailbox(&auth(), dropped.id()).expect("the live route retires");
    assert!(!cast(starting), "a Starting route is not live");
    assert!(!cast(dropped), "a dropped route is not live");
    assert!(!cast_unchecked(starting), "a Starting route cannot mint the unchecked protocol");
    assert!(!cast_unchecked(dropped), "a dropped route cannot mint the unchecked protocol");
}
