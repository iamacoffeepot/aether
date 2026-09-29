//! Shared route member sets (ADR-0136 / issue 2625): two claimants of one
//! prefix alternate round-robin, the typed `#[http::router(shared)]` opt-in
//! threads all the way into the registration send, and a bare router stays
//! exclusive so a second claim is rejected.

use aether_substrate::Subname;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::testing::{TestChassis, fresh_substrate};
use std::sync::Arc;

use crate::server::{HttpServerCapability, HttpServerConfig};

use super::handlers::{
    ExclusiveMacroPoolHandler, FixedBodyHttpHandler, SharedAlphaHandler, SharedBetaHandler, SharedMacroPoolHandler,
};
use super::support::{body_of, boot_single_shard_fixed_body, port_of, round_trip};

/// A bare `#[http::router]` impl still registers exclusive (issue 2625
/// regression guard on the default): two instances of the same compiled
/// actor claim `/macro-excl` through the typed macro surface with no
/// `shared` argument. Both run the same macro-emitted registration, so an
/// accidental `shared: true` default would let both instances serve; the
/// exclusive default keeps the route owned by exactly one instance. Each
/// spawn waits on its own wire root, so alpha's claim settles before beta's
/// is sent and alpha owns the route.
#[test]
fn bare_router_stays_exclusive_second_claim_rejected() {
    let chassis = boot_single_shard_fixed_body();
    chassis
        .spawn_actor::<ExclusiveMacroPoolHandler>(Subname::Named("alpha"), b"excl-macro-alpha", ())
        .finish_wire_settled()
        .expect("spawn alpha");
    chassis
        .spawn_actor::<ExclusiveMacroPoolHandler>(Subname::Named("beta"), b"excl-macro-beta", ())
        .finish_wire_settled()
        .expect("spawn beta");
    let port = port_of(&chassis);

    for _ in 0..25 {
        let contested = round_trip(port, b"GET /macro-excl HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(
            body_of(&contested),
            "excl-macro-alpha",
            "bare #[http::router] must stay exclusive to its first claimant; full response: {contested:?}",
        );
    }
}

/// `#[http::router(shared)]` (issue 2625) threads the flag from the
/// attribute all the way into the wire `RegisterRouteSelf` send: two
/// instances of a component built with the opt-in join one round-robin
/// member set and both serve — the bug this catches is the flag failing
/// to thread, which would make the second instance's registration a
/// conflict `Err` instead of a join (only one tag would ever serve).
#[test]
fn macro_router_shared_opt_in_joins_a_member_set() {
    // Pinned to one dispatch shard, like `shared_route_spreads_across_members`:
    // the round-robin cursor is per connection, seeded from the shard's own
    // `next_conn_id`, and each `round_trip` opens a new connection, so
    // alternation across a request sequence follows one shard's conn-id
    // sequence only when a single shard takes every connection.
    let chassis = boot_single_shard_fixed_body();
    // Two named instances of the exact same `SharedMacroPoolHandler` type
    // (the accurate replica analog, per the type's own doc comment): each
    // instance's `wire` runs the identical macro-emitted `shared: true`
    // registration, so both join one member set.
    chassis
        .spawn_actor::<SharedMacroPoolHandler>(Subname::Named("alpha"), b"macro-alpha", ())
        .finish_wire_settled()
        .expect("spawn alpha");
    chassis
        .spawn_actor::<SharedMacroPoolHandler>(Subname::Named("beta"), b"macro-beta", ())
        .finish_wire_settled()
        .expect("spawn beta");
    let port = port_of(&chassis);

    // Both registrations settled before the first request, so six requests
    // alternate from any starting cursor: both members serve, and only
    // members serve.
    let mut alpha = 0;
    let mut beta = 0;
    for _ in 0..6 {
        let response = round_trip(port, b"GET /macro-pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        match body_of(&response) {
            "macro-alpha" => alpha += 1,
            "macro-beta" => beta += 1,
            other => panic!("unexpected /macro-pool body {other:?}"),
        }
    }
    assert_eq!((alpha, beta), (3, 3), "round-robin alternation over 6 requests");
}

/// A shared member set (ADR-0136) spreads requests across its members
/// round-robin: alpha and beta both register `/pool` shared, and with
/// `dispatch_shards` pinned to 1 (one conn-id sequence seeding each
/// connection's cursor) sequential requests alternate between them — both
/// bodies observed, nothing else.
///
/// Tripwire: without member sets the second shared claim is rejected
/// and every request serves "alpha"; with a broken cursor (never
/// advancing) likewise.
#[test]
fn shared_route_spreads_across_members() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<HttpServerCapability>(
            (),
            HttpServerConfig {
                enabled: true,
                bind_addr: "127.0.0.1:0".to_string(),
                request_timeout_millis: 5_000,
                dispatch_shards: 1,
                ..HttpServerConfig::default()
            },
        )
        .with_actor::<FixedBodyHttpHandler>(())
        .with_actor::<SharedAlphaHandler>(())
        .with_actor::<SharedBetaHandler>(())
        .build_passive()
        .expect("caps boot");
    chassis.await_boot_settled();
    let port = port_of(&chassis);

    // Both registrations settled before the first request, so six requests
    // alternate from any starting cursor: both members serve, and only
    // members serve.
    let mut alpha = 0;
    let mut beta = 0;
    for _ in 0..6 {
        let response = round_trip(port, b"GET /pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        match body_of(&response) {
            "alpha" => alpha += 1,
            "beta" => beta += 1,
            other => panic!("unexpected /pool body {other:?}"),
        }
    }
    assert_eq!((alpha, beta), (3, 3), "round-robin alternation over 6 requests");
}
