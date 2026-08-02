//! Shared route member sets (ADR-0136 / issue 2625): two claimants of one
//! prefix alternate round-robin, the typed `#[http::router(shared)]` opt-in
//! threads all the way into the registration send, and a bare router stays
//! exclusive so a second claim is rejected.

use aether_substrate::Subname;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::testing::{TestChassis, fresh_substrate};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::server::{HttpServerCapability, HttpServerConfig};

use super::handlers::{
    ExclusiveMacroPoolHandler, FixedBodyHttpHandler, SharedAlphaHandler, SharedBetaHandler, SharedMacroPoolHandler,
};
use super::support::{body_of, boot_single_shard_fixed_body, port_of, round_trip};

/// A bare `#[http::router]` impl still registers exclusive (issue 2625
/// regression guard on the default): two instances of the same compiled
/// actor claim `/macro-excl` through the typed macro surface with no
/// `shared` argument. Because they share the same macro-minted kind, an
/// accidental `shared: true` default would let both instances serve; the
/// exclusive default keeps the route owned by exactly one instance.
#[test]
fn bare_router_stays_exclusive_second_claim_rejected() {
    let chassis = boot_single_shard_fixed_body();
    chassis
        .spawn_actor::<ExclusiveMacroPoolHandler>(Subname::Named("alpha"), b"excl-macro-alpha", ())
        .finish()
        .expect("spawn alpha");
    chassis
        .spawn_actor::<ExclusiveMacroPoolHandler>(Subname::Named("beta"), b"excl-macro-beta", ())
        .finish()
        .expect("spawn beta");
    let port = port_of(&chassis);

    let deadline = Instant::now() + Duration::from_secs(10);
    let owner = loop {
        let contested = round_trip(port, b"GET /macro-excl HTTP/1.1\r\nHost: localhost\r\n\r\n");
        match body_of(&contested) {
            "excl-macro-alpha" | "excl-macro-beta" => break body_of(&contested).to_string(),
            _ => {
                assert!(Instant::now() < deadline, "expected /macro-excl to become live within 10s");
                thread::sleep(Duration::from_millis(25));
            }
        }
    };

    for _ in 0..24 {
        let contested = round_trip(port, b"GET /macro-excl HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let body = body_of(&contested);
        assert_eq!(body, owner, "bare #[http::router] must stay exclusive; observed a second route member");
        thread::sleep(Duration::from_millis(10));
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
    // round-robin state is per-shard, so alternation across a request
    // sequence is only deterministic with a single shard.
    let chassis = boot_single_shard_fixed_body();
    // Two named instances of the exact same `SharedMacroPoolHandler` type
    // (the accurate replica analog, per the type's own doc comment): each
    // instance's `wire` runs the identical macro-emitted `shared: true`
    // registration, so both carry the same minted `Kind::ID` and can join
    // one member set.
    chassis
        .spawn_actor::<SharedMacroPoolHandler>(Subname::Named("alpha"), b"macro-alpha", ())
        .finish()
        .expect("spawn alpha");
    chassis
        .spawn_actor::<SharedMacroPoolHandler>(Subname::Named("beta"), b"macro-beta", ())
        .finish()
        .expect("spawn beta");
    let port = port_of(&chassis);

    // Wait until both registrations are live: with the set complete a
    // pair of consecutive requests serves both bodies.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let first = round_trip(port, b"GET /macro-pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let second = round_trip(port, b"GET /macro-pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let pair = [body_of(&first).to_string(), body_of(&second).to_string()];
        if pair.contains(&"macro-alpha".to_string()) && pair.contains(&"macro-beta".to_string()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected macro-alpha+macro-beta across a request pair within 10s; got {pair:?}",
        );
        thread::sleep(Duration::from_millis(25));
    }

    // Steady state: six more requests keep alternating — both members
    // serve, and only members serve.
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
/// `dispatch_shards` pinned to 1 (one cursor) sequential requests
/// alternate between them — both bodies observed, nothing else.
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
    let port = port_of(&chassis);

    // Wait until both registrations are live: with the set complete a
    // pair of consecutive requests serves both bodies.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let first = round_trip(port, b"GET /pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let second = round_trip(port, b"GET /pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let pair = [body_of(&first).to_string(), body_of(&second).to_string()];
        if pair.contains(&"alpha".to_string()) && pair.contains(&"beta".to_string()) {
            break;
        }
        assert!(Instant::now() < deadline, "expected alpha+beta across a request pair within 10s; got {pair:?}");
        thread::sleep(Duration::from_millis(25));
    }

    // Steady state: six more requests keep alternating — both members
    // serve, and only members serve.
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
