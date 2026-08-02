//! The supervisor's own surface: binding (and the disabled server that binds
//! nothing), the global connection ceiling, dispatch-shard assignment and its
//! activation-failure paths (ADR-0135), and the reader-thread isolation that
//! keeps a stalled peer off the shard.

use aether_actor::Addressable;
use aether_data::{MailboxId, mailbox_id_from_path};
use aether_substrate::chassis::builder::Builder;
use aether_substrate::mail::registry::{OwnedDispatch, Registry};
use aether_substrate::testing::{TestChassis, boot_authority, fresh_substrate};
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::server::shard::HttpDispatchShard;
use crate::server::{HttpServerCapability, HttpServerConfig, HttpServerHandle};

use super::handlers::EchoHttpHandler;
use super::support::{boot_chassis, config_for, port_of, read_one_response, round_trip, round_trip_live};

fn shard_canonical_name(index: usize) -> String {
    format!(
        "{}/{}:shard-{index}",
        <HttpServerCapability as Addressable>::NAMESPACE,
        <HttpDispatchShard as Addressable>::NAMESPACE,
    )
}

fn register_shard_collision(registry: &Registry, index: usize) -> MailboxId {
    let canonical_name = shard_canonical_name(index);
    let id = mailbox_id_from_path(&canonical_name);
    registry
        .try_register_inbox_with_id(
            &boot_authority(),
            id,
            canonical_name,
            Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()),
        )
        .expect("install test-only dispatch-shard collision authority");
    id
}

/// The light non-contention test: the cap binds and publishes the bound
/// port.
#[test]
fn binds_and_publishes_port() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<HttpServerCapability>((), config_for(1024))
        .build_passive()
        .expect("http server boots");
    assert!(port_of(&chassis) > 0, "bound to an OS-picked port");
}

/// ADR-0155 §3: a server composed disabled (`enabled: false`, the config
/// default) still claims its `aether.http.server` mailbox — so mail to it
/// is diagnosable rather than warn-dropped at an unknown mailbox — but
/// binds no socket, so no `HttpServerHandle` (and hence no listener port)
/// is published.
#[test]
fn disabled_http_server_claims_mailbox_and_binds_nothing() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<HttpServerCapability>((), HttpServerConfig::default())
        .build_passive()
        .expect("disabled http server boots");

    assert!(
        registry.lookup(<HttpServerCapability as Addressable>::NAMESPACE).is_some(),
        "a disabled http server still claims its mailbox",
    );
    assert!(
        chassis.handle::<HttpServerHandle>().is_none(),
        "a disabled http server binds no socket, so it publishes no handle",
    );
}

/// A peer accepted past `max_connections` is refused a canned `503`
/// and closed before a reader thread is spawned; it never reaches the
/// handler. `dispatch_shards` is pinned above one so the ceiling is
/// provably global across shards (ADR-0135) — the two held connections
/// land on different shards round-robin, and the supervisor still
/// refuses the third against the shared live count.
///
/// Tripwire: without the assignment-time capacity guard in
/// `HttpSupervisorState::assign_peer`, this connection is accepted and
/// dispatched (or hangs waiting on the handler) instead of being
/// refused; with a per-shard rather than global count, it is accepted
/// because no single shard is at the ceiling.
#[test]
fn over_capacity_connection_is_503() {
    let max_connections = 2;
    let chassis = boot_chassis::<EchoHttpHandler>(HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        max_connections,
        dispatch_shards: 2,
        ..HttpServerConfig::default()
    });

    let port = port_of(&chassis);

    // Fill the connection table: each socket sends a partial request
    // head (no terminating blank line), so its reader thread blocks
    // waiting for more bytes and its `ConnState` stays resident.
    let mut held = Vec::new();
    for _ in 0..max_connections {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
        stream.write_all(b"GET / HTTP/1.1\r\n").expect("write partial request head");
        stream.flush().expect("flush partial request head");
        held.push(stream);
    }

    // Give the dispatcher a moment to drain the `PeerAccepted` events
    // into `connections` before the next connect.
    thread::sleep(Duration::from_millis(200));

    let response = round_trip(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 503 "), "expected 503, got: {response:?}");

    drop(held);
}

/// Concurrent connections under a pinned multi-shard config (ADR-0135) are
/// all served, including keep-alive reuse: four connections land on two
/// shards round-robin, each serves two sequential requests, every reply
/// routes back through the owning shard. Pinned to `dispatch_shards: 2`
/// rather than the auto worker-count sizing so the multi-shard path is
/// exercised even on a low-core CI runner where auto sizing collapses to
/// one shard.
///
/// Tripwire: a shard-assignment bug (posting to a never-spawned shard, a
/// round-robin index error, a reply intercepted by the wrong actor)
/// surfaces here as a hung read or a `502`/`503` on some subset of the
/// connections.
#[test]
fn connections_distribute_across_shards() {
    let chassis = boot_chassis::<EchoHttpHandler>(HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        dispatch_shards: 2,
        ..HttpServerConfig::default()
    });

    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before driving the concurrent
    // connections so none of them races the registration.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut streams: Vec<(TcpStream, Vec<u8>)> = (0..4)
        .map(|_| {
            let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
            (stream, Vec::new())
        })
        .collect();

    for round in 0..2 {
        // Write this round's request on every connection first, then read
        // every response — so all four connections are in flight across
        // both shards at once, not serialized one connection at a time.
        for (index, (stream, _)) in streams.iter_mut().enumerate() {
            let request = format!("GET /conn{index}/round{round} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            stream.write_all(request.as_bytes()).expect("write request");
            stream.flush().expect("flush request");
        }
        for (index, (stream, carry)) in streams.iter_mut().enumerate() {
            let response = read_one_response(stream, carry);
            assert!(
                response.starts_with("HTTP/1.1 200 "),
                "conn {index} round {round}: expected 200, got: {response:?}",
            );
            assert!(
                response.contains(&format!("x-aether-path: /conn{index}/round{round}")),
                "conn {index} round {round}: reply correlated to the wrong request: {response:?}",
            );
        }
    }
}

/// Scheduler-backed partial apply rejection: index zero collides under
/// explicit test authority, index one activates, and the cold first request
/// remains supervisor-owned until both attempts settle. The surviving shard
/// then serves it; completion order cannot turn the rejected slot into a
/// selectable sink or lose the trigger connection.
#[test]
fn cold_first_request_survives_partial_shard_activation_failure() {
    let (registry, mailer) = fresh_substrate();
    let collision = register_shard_collision(&registry, 0);
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor_configured::<HttpServerCapability>(
            (),
            HttpServerConfig {
                enabled: true,
                bind_addr: "127.0.0.1:0".to_string(),
                request_timeout_millis: 5_000,
                dispatch_shards: 2,
                ..HttpServerConfig::default()
            },
        )
        .build_passive()
        .expect("http server boots with test-only shard collision");

    let response = round_trip_live(port_of(&chassis), b"GET /cold HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 "), "surviving shard serves the cold peer: {response:?}");
    let surviving = shard_canonical_name(1);
    assert_eq!(
        registry.lookup(&surviving),
        Some(mailbox_id_from_path(&surviving)),
        "the successful deterministic index is Live before the retained peer is dispatched",
    );

    drop(chassis);
    registry.drop_mailbox(&boot_authority(), collision).expect("remove test-only shard collision");
}

/// Scheduler-backed total apply rejection: every staged shard loses its
/// canonical-name claim, so the retained first socket receives one controlled
/// `503`. The supervisor stays `Failed`; a later peer receives the same
/// refusal without implicitly spawning another generation.
#[test]
fn all_shard_activation_failures_refuse_without_implicit_retry() {
    let (registry, mailer) = fresh_substrate();
    let collisions = [register_shard_collision(&registry, 0), register_shard_collision(&registry, 1)];
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor_configured::<HttpServerCapability>(
            (),
            HttpServerConfig {
                enabled: true,
                bind_addr: "127.0.0.1:0".to_string(),
                request_timeout_millis: 5_000,
                dispatch_shards: 2,
                ..HttpServerConfig::default()
            },
        )
        .build_passive()
        .expect("http server boots with test-only shard collisions");
    let port = port_of(&chassis);

    let first = round_trip(port, b"GET /first HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(first.starts_with("HTTP/1.1 503 "), "cold retained peer receives controlled refusal: {first:?}");
    let later = round_trip(port, b"GET /later HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(later.starts_with("HTTP/1.1 503 "), "failed startup does not retry implicitly: {later:?}");

    drop(chassis);
    for collision in collisions {
        registry.drop_mailbox(&boot_authority(), collision).expect("remove test-only shard collision");
    }
}

/// A peer that stalls its receive window blocks only its own reader
/// thread, never the dispatch shard (ADR-0135 §3): with one shard
/// pinned, connection A parks a 16 MiB echo response against a client
/// that refuses to read while connection B's small request round-trips
/// promptly through the same shard.
///
/// Tripwire: with the response write back on the shard's dispatch (the
/// pre-ADR-0135 §3 shape), A's blocked `write_all` freezes the shard
/// and B times out empty.
#[test]
fn stalled_peer_does_not_block_sibling_connections() {
    let chassis = boot_chassis::<EchoHttpHandler>(HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        // Short: the stalled write parks within milliseconds, and
        // teardown waits out at most one response deadline.
        request_timeout_millis: 2_000,
        max_request_bytes: 32 * 1024 * 1024,
        dispatch_shards: 1,
        ..HttpServerConfig::default()
    });
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the stall setup, so both
    // connections dispatch to the echo handler rather than racing it.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    // Connection A: a 16 MiB echo whose response the client never
    // reads — far past loopback socket buffering, so the reader's
    // write_all parks against A's receive window.
    let body_len = 16 * 1024 * 1024;
    let mut stalled = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect stalled peer");
    let mut request =
        format!("POST /stall HTTP/1.1\r\nHost: localhost\r\nContent-Length: {body_len}\r\n\r\n").into_bytes();
    request.resize(request.len() + body_len, b'a');
    stalled.write_all(&request).expect("write stalled request");
    stalled.flush().expect("flush stalled request");

    // Give the echo time to dispatch and its response write to park.
    thread::sleep(Duration::from_millis(300));

    // Connection B on the same (sole) shard round-trips promptly.
    let response = round_trip(port, b"GET /probe HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(
        response.starts_with("HTTP/1.1 200 "),
        "sibling connection must be served while a peer stalls; got: {response:?}",
    );

    drop(stalled);
}
